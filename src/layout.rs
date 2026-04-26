use std::io;

use crate::{config::Config, encode, scan, term::Protocol};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Processed {
    pub bytes: Vec<u8>,
    pub rewritten: bool,
    pub passthrough: bool,
    pub interactive: Option<InteractiveOutput>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InteractiveOutput {
    pub prefix: Vec<u8>,
    pub rows: Vec<Vec<u8>>,
    pub suffix: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Aligned,
    Unaligned,
    Expanded,
}

pub fn process(input: &[u8], protocol: Protocol, config: &Config) -> io::Result<Processed> {
    if config.disable || protocol == Protocol::None {
        return Ok(passthrough(input));
    }

    let lines = split_inclusive_lines(input);
    let mut candidate_lines = Vec::new();
    let mut too_large = false;

    for (line_index, line) in lines.iter().enumerate() {
        if line.bytes.len() > config.max_row_bytes {
            if line.bytes.windows(2).any(|window| window == b"\\x") {
                too_large = true;
                break;
            }
            continue;
        }

        match scan::image_tokens(line.bytes, config.max_row_bytes) {
            Ok(tokens) if !tokens.is_empty() => candidate_lines.push((line_index, tokens)),
            Ok(_) => {}
            Err(scan::ScanError::TokenTooLarge) => {
                too_large = true;
                break;
            }
        }
    }

    if too_large || candidate_lines.is_empty() {
        return Ok(passthrough(input));
    }

    let mode = match classify(&lines, &candidate_lines) {
        Some(mode) => mode,
        None => return Ok(passthrough(input)),
    };

    let interactive = build_interactive_output(&lines, &candidate_lines, mode, protocol, config)?;
    let out = interactive.bytes();

    Ok(Processed {
        bytes: out,
        rewritten: true,
        passthrough: false,
        interactive: Some(interactive),
    })
}

fn passthrough(input: &[u8]) -> Processed {
    Processed {
        bytes: input.to_vec(),
        rewritten: false,
        passthrough: true,
        interactive: None,
    }
}

impl InteractiveOutput {
    pub fn bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            self.prefix.len() + self.suffix.len() + self.rows.iter().map(Vec::len).sum::<usize>(),
        );
        out.extend_from_slice(&self.prefix);
        for row in &self.rows {
            out.extend_from_slice(row);
        }
        out.extend_from_slice(&self.suffix);
        out
    }
}

fn classify(lines: &[Line<'_>], candidate_lines: &[(usize, Vec<scan::Token>)]) -> Option<Mode> {
    if looks_expanded(lines) {
        return expanded_is_single_column(lines).then_some(Mode::Expanded);
    }

    if looks_aligned(lines) {
        return candidate_lines
            .iter()
            .all(|(line_index, _)| !lines[*line_index].content_without_newline().contains(&b'|'))
            .then_some(Mode::Aligned);
    }

    candidate_lines
        .iter()
        .all(|(line_index, _)| !lines[*line_index].content_without_newline().contains(&b'|'))
        .then_some(Mode::Unaligned)
}

fn looks_expanded(lines: &[Line<'_>]) -> bool {
    lines
        .iter()
        .any(|line| line.content_without_newline().starts_with(b"-[ RECORD"))
}

fn expanded_is_single_column(lines: &[Line<'_>]) -> bool {
    let mut fields_in_record = 0usize;
    let mut saw_record = false;
    let mut saw_value = false;

    for line in lines {
        let content = line.content_without_newline();
        if content.starts_with(b"-[ RECORD") {
            if saw_record && fields_in_record > 1 {
                return false;
            }
            saw_record = true;
            fields_in_record = 0;
            continue;
        }

        if content.trim_ascii().starts_with(b"(") || content.trim_ascii().is_empty() {
            continue;
        }

        if content.windows(3).any(|window| window == b" | ") {
            fields_in_record += 1;
            saw_value = true;
        }
    }

    saw_record && saw_value && fields_in_record <= 1
}

fn looks_aligned(lines: &[Line<'_>]) -> bool {
    lines.iter().any(|line| {
        let content = line.content_without_newline().trim_ascii();
        !content.is_empty()
            && content
                .iter()
                .all(|byte| matches!(byte, b'-' | b'+' | b' '))
            && content.contains(&b'-')
    })
}

fn rewrite_line(
    out: &mut Vec<u8>,
    line: &[u8],
    tokens: &[scan::Token],
    mode: Mode,
    protocol: Protocol,
    config: &Config,
) -> io::Result<()> {
    let mut cursor = 0;
    for token in tokens {
        out.extend_from_slice(&line[cursor..token.start]);
        match mode {
            Mode::Aligned => write_aligned_placeholder(out, token.end - token.start),
            Mode::Unaligned | Mode::Expanded => out.extend_from_slice(b"[img]"),
        }
        cursor = token.end;
    }
    out.extend_from_slice(&line[cursor..]);

    for token in tokens {
        let original = std::str::from_utf8(&line[token.start..token.end]).unwrap_or("");
        encode::write(out, protocol, original, &token.decoded, config)?;
        if !out.ends_with(b"\n") {
            out.push(b'\n');
        }
    }

    Ok(())
}

fn build_interactive_output(
    lines: &[Line<'_>],
    candidate_lines: &[(usize, Vec<scan::Token>)],
    mode: Mode,
    protocol: Protocol,
    config: &Config,
) -> io::Result<InteractiveOutput> {
    match mode {
        Mode::Aligned | Mode::Unaligned => {
            build_tabular_interactive_output(lines, candidate_lines, mode, protocol, config)
        }
        Mode::Expanded => {
            build_expanded_interactive_output(lines, candidate_lines, protocol, config)
        }
    }
}

fn build_tabular_interactive_output(
    lines: &[Line<'_>],
    candidate_lines: &[(usize, Vec<scan::Token>)],
    mode: Mode,
    protocol: Protocol,
    config: &Config,
) -> io::Result<InteractiveOutput> {
    let first_candidate = candidate_lines[0].0;
    let last_candidate = candidate_lines[candidate_lines.len() - 1].0;

    let mut prefix = Vec::new();
    for line in &lines[..first_candidate] {
        prefix.extend_from_slice(line.bytes);
    }

    let mut rows = Vec::with_capacity(last_candidate - first_candidate + 1);
    for (line_index, line) in lines
        .iter()
        .enumerate()
        .take(last_candidate + 1)
        .skip(first_candidate)
    {
        let mut row = Vec::new();
        if let Some(tokens) = candidate_tokens(candidate_lines, line_index) {
            rewrite_line(&mut row, line.bytes, tokens, mode, protocol, config)?;
        } else {
            row.extend_from_slice(line.bytes);
        }
        rows.push(row);
    }

    let mut suffix = Vec::new();
    for line in &lines[last_candidate + 1..] {
        suffix.extend_from_slice(line.bytes);
    }

    Ok(InteractiveOutput {
        prefix,
        rows,
        suffix,
    })
}

fn build_expanded_interactive_output(
    lines: &[Line<'_>],
    candidate_lines: &[(usize, Vec<scan::Token>)],
    protocol: Protocol,
    config: &Config,
) -> io::Result<InteractiveOutput> {
    let mut prefix = Vec::new();
    let first_candidate = candidate_lines[0].0;
    let last_candidate = candidate_lines[candidate_lines.len() - 1].0;
    let first_record_start = record_start_for(lines, first_candidate, 0);
    let last_record_start = record_start_for(lines, last_candidate, first_record_start);
    let last_record_end = next_record_start(lines, last_record_start + 1);

    for line in &lines[..first_record_start] {
        prefix.extend_from_slice(line.bytes);
    }

    let mut rows = Vec::with_capacity(candidate_lines.len());
    let mut row_start = first_record_start;
    while row_start < last_record_end {
        let row_end = next_record_start(lines, row_start + 1).min(last_record_end);

        let mut row = Vec::new();
        for (line_index, line) in lines.iter().enumerate().take(row_end).skip(row_start) {
            if let Some(tokens) = candidate_tokens(candidate_lines, line_index) {
                rewrite_line(
                    &mut row,
                    line.bytes,
                    tokens,
                    Mode::Expanded,
                    protocol,
                    config,
                )?;
            } else {
                row.extend_from_slice(line.bytes);
            }
        }
        rows.push(row);
        row_start = row_end;
    }

    let mut suffix = Vec::new();
    for line in &lines[last_record_end..] {
        suffix.extend_from_slice(line.bytes);
    }

    Ok(InteractiveOutput {
        prefix,
        rows,
        suffix,
    })
}

fn candidate_tokens(
    candidate_lines: &[(usize, Vec<scan::Token>)],
    line_index: usize,
) -> Option<&[scan::Token]> {
    candidate_lines
        .iter()
        .find_map(|(candidate_index, tokens)| (*candidate_index == line_index).then_some(&**tokens))
}

fn record_start_for(lines: &[Line<'_>], line_index: usize, stop_at: usize) -> usize {
    let mut index = line_index;
    loop {
        if is_record_start(lines[index]) || index == stop_at {
            return index;
        }
        index -= 1;
    }
}

fn next_record_start(lines: &[Line<'_>], start_at: usize) -> usize {
    lines[start_at..]
        .iter()
        .position(|line| is_record_start(*line))
        .map_or(lines.len(), |offset| start_at + offset)
}

fn is_record_start(line: Line<'_>) -> bool {
    line.content_without_newline().starts_with(b"-[ RECORD")
}

fn write_aligned_placeholder(out: &mut Vec<u8>, width: usize) {
    let placeholder = b"[img]";
    out.extend_from_slice(placeholder);
    if width > placeholder.len() {
        out.extend(std::iter::repeat_n(b' ', width - placeholder.len()));
    }
}

#[derive(Clone, Copy)]
struct Line<'a> {
    bytes: &'a [u8],
}

impl Line<'_> {
    fn content_without_newline(&self) -> &[u8] {
        self.bytes.strip_suffix(b"\n").unwrap_or(self.bytes)
    }
}

fn split_inclusive_lines(input: &[u8]) -> Vec<Line<'_>> {
    if input.is_empty() {
        return Vec::new();
    }

    let mut lines = Vec::new();
    let mut start = 0;
    for (index, byte) in input.iter().enumerate() {
        if *byte == b'\n' {
            lines.push(Line {
                bytes: &input[start..=index],
            });
            start = index + 1;
        }
    }
    if start < input.len() {
        lines.push(Line {
            bytes: &input[start..],
        });
    }
    lines
}

#[cfg(test)]
mod tests {
    use crate::{
        config::Config,
        layout::process,
        scan::{GIF89A_MAGIC, PNG_MAGIC},
        term::Protocol,
    };

    fn config() -> Config {
        Config {
            max_row_bytes: 4096,
            max_pixels_w: None,
            max_pixels_h: None,
            disable: false,
            fallback: None,
        }
    }

    fn png_hex() -> String {
        let mut out = String::from("\\x");
        for byte in PNG_MAGIC {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }

    fn gif_hex() -> String {
        let mut out = String::from("\\x");
        for byte in GIF89A_MAGIC {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }

    #[test]
    fn rewrites_single_column_unaligned() {
        let input = format!("thumbnail\n{}\n(1 row)\n", png_hex());
        let processed = process(input.as_bytes(), Protocol::Kitty, &config()).unwrap();
        let text = String::from_utf8(processed.bytes).unwrap();
        assert!(processed.rewritten);
        assert!(text.contains("[img]\n\x1b_Gf=100,a=T;"));
    }

    #[test]
    fn rewrites_gif_single_column_unaligned() {
        let input = format!("thumbnail\n{}\n(1 row)\n", gif_hex());
        let processed = process(input.as_bytes(), Protocol::ITerm2, &config()).unwrap();
        let text = String::from_utf8(processed.bytes).unwrap();
        assert!(processed.rewritten);
        assert!(text.contains("[img]\n\x1b]1337;File=inline=1;size=6;"));
    }

    #[test]
    fn rewrites_single_column_aligned() {
        let input = format!(" thumbnail \n-----------\n {} \n(1 row)\n", png_hex());
        let processed = process(input.as_bytes(), Protocol::Kitty, &config()).unwrap();
        let text = String::from_utf8(processed.bytes).unwrap();
        assert!(processed.rewritten);
        assert!(text.contains(" [img]"));
        assert!(text.contains("\x1b_Gf=100,a=T;"));
    }

    #[test]
    fn separates_aligned_rewrites_into_interactive_rows() {
        let input = format!(
            " thumbnail \n-----------\n {} \n {} \n(2 rows)\n",
            png_hex(),
            png_hex()
        );
        let processed = process(input.as_bytes(), Protocol::Kitty, &config()).unwrap();
        let interactive = processed.interactive.as_ref().unwrap();

        assert_eq!(interactive.rows.len(), 2);
        assert_eq!(interactive.prefix, b" thumbnail \n-----------\n");
        assert_eq!(interactive.suffix, b"(2 rows)\n");
        assert!(String::from_utf8(interactive.rows[0].clone())
            .unwrap()
            .contains(" [img]"));
        assert_eq!(interactive.bytes(), processed.bytes);
    }

    #[test]
    fn keeps_non_image_tabular_rows_between_rewrites() {
        let input = format!(
            " thumbnail \n-----------\n {} \n text row \n {} \n(3 rows)\n",
            png_hex(),
            png_hex()
        );
        let processed = process(input.as_bytes(), Protocol::Kitty, &config()).unwrap();
        let interactive = processed.interactive.as_ref().unwrap();

        assert_eq!(interactive.rows.len(), 3);
        assert_eq!(interactive.rows[1], b" text row \n");
        assert!(String::from_utf8(processed.bytes.clone())
            .unwrap()
            .contains(" text row \n"));
        assert_eq!(interactive.bytes(), processed.bytes);
    }

    #[test]
    fn rewrites_single_column_expanded() {
        let input = format!("-[ RECORD 1 ]-----\nthumbnail | {}\n", png_hex());
        let processed = process(input.as_bytes(), Protocol::ITerm2, &config()).unwrap();
        let text = String::from_utf8(processed.bytes).unwrap();
        assert!(processed.rewritten);
        assert!(text.contains("thumbnail | [img]\n\x1b]1337;File="));
    }

    #[test]
    fn separates_expanded_records_into_interactive_rows() {
        let input = format!(
            "-[ RECORD 1 ]-----\nthumbnail | {}\n-[ RECORD 2 ]-----\nthumbnail | {}\n",
            png_hex(),
            png_hex()
        );
        let processed = process(input.as_bytes(), Protocol::ITerm2, &config()).unwrap();
        let interactive = processed.interactive.as_ref().unwrap();

        assert_eq!(interactive.rows.len(), 2);
        assert!(interactive.prefix.is_empty());
        assert!(interactive.suffix.is_empty());
        assert!(String::from_utf8(interactive.rows[0].clone())
            .unwrap()
            .starts_with("-[ RECORD 1 ]-----\nthumbnail | [img]\n"));
        assert!(String::from_utf8(interactive.rows[1].clone())
            .unwrap()
            .starts_with("-[ RECORD 2 ]-----\nthumbnail | [img]\n"));
        assert_eq!(interactive.bytes(), processed.bytes);
    }

    #[test]
    fn keeps_non_image_expanded_records_between_rewrites() {
        let input = format!(
            "-[ RECORD 1 ]-----\nthumbnail | {}\n-[ RECORD 2 ]-----\nthumbnail | text row\n-[ RECORD 3 ]-----\nthumbnail | {}\n",
            png_hex(),
            png_hex()
        );
        let processed = process(input.as_bytes(), Protocol::ITerm2, &config()).unwrap();
        let interactive = processed.interactive.as_ref().unwrap();

        assert_eq!(interactive.rows.len(), 3);
        assert_eq!(
            interactive.rows[1],
            b"-[ RECORD 2 ]-----\nthumbnail | text row\n"
        );
        assert!(String::from_utf8(processed.bytes.clone())
            .unwrap()
            .contains("-[ RECORD 2 ]-----\nthumbnail | text row\n"));
        assert_eq!(interactive.bytes(), processed.bytes);
    }

    #[test]
    fn multi_column_unaligned_passthrough() {
        let input = format!("thumbnail|id\n{}|1\n(1 row)\n", png_hex());
        let processed = process(input.as_bytes(), Protocol::Kitty, &config()).unwrap();
        assert!(processed.passthrough);
        assert_eq!(processed.bytes, input.as_bytes());
    }

    #[test]
    fn multi_column_aligned_passthrough() {
        let input = format!(
            " thumbnail | id \n-----------+----\n {} | 1\n(1 row)\n",
            png_hex()
        );
        let processed = process(input.as_bytes(), Protocol::Kitty, &config()).unwrap();
        assert!(processed.passthrough);
        assert_eq!(processed.bytes, input.as_bytes());
    }

    #[test]
    fn multi_column_expanded_passthrough() {
        let input = format!(
            "-[ RECORD 1 ]-----\nthumbnail | {}\nid        | 1\n",
            png_hex()
        );
        let processed = process(input.as_bytes(), Protocol::Kitty, &config()).unwrap();
        assert!(processed.passthrough);
        assert_eq!(processed.bytes, input.as_bytes());
    }

    #[test]
    fn none_protocol_passthrough() {
        let input = format!("{}\n", png_hex());
        let processed = process(input.as_bytes(), Protocol::None, &config()).unwrap();
        assert!(processed.passthrough);
        assert_eq!(processed.bytes, input.as_bytes());
    }

    #[test]
    fn row_over_cap_passthrough() {
        let input = format!("{}\n", png_hex());
        let mut config = config();
        config.max_row_bytes = 8;
        let processed = process(input.as_bytes(), Protocol::Kitty, &config).unwrap();
        assert!(processed.passthrough);
        assert_eq!(processed.bytes, input.as_bytes());
    }
}
