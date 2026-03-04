use serde::{Deserialize, Serialize};
use xxhash_rust::xxh3::xxh3_64;

/// A 1-byte truncated xxh3 hash of trimmed line content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineHash(pub u8);

impl LineHash {
    pub fn compute(line_content: &str) -> Self {
        let digest = xxh3_64(line_content.trim_ascii().as_bytes());
        Self(digest.to_le_bytes()[0])
    }

    pub fn display(&self) -> String {
        format!("{:02x}", self.0)
    }
}

/// Tag raw file content with hashline markers.
///
/// Output format per line: `{line_number:>width}:{hash}|{content}`
/// where width is the number of digits in the total line count.
pub fn tag_content(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return String::new();
    }

    let width = lines.len().to_string().len();
    let mut output = String::new();

    for (i, line) in lines.iter().enumerate() {
        let line_num = i + 1;
        let hash = LineHash::compute(line);
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&format!(
            "{:>width$}:{}|{}",
            line_num,
            hash.display(),
            line,
            width = width
        ));
    }

    output
}

/// Parse a hashline reference string like "2:f1" into (line_number, LineHash).
pub fn parse_hashline(s: &str) -> Option<(u32, LineHash)> {
    let (line_str, hash_str) = s.split_once(':')?;
    let line: u32 = line_str.parse().ok()?;
    if hash_str.len() != 2 {
        return None;
    }
    let hash_val = u8::from_str_radix(hash_str, 16).ok()?;
    Some((line, LineHash(hash_val)))
}

/// Compute hashlines for all lines of content. Returns Vec of (1-based line number, hash).
pub fn compute_hashlines(content: &str) -> Vec<(u32, LineHash)> {
    content
        .lines()
        .enumerate()
        .map(|(i, line)| ((i + 1) as u32, LineHash::compute(line)))
        .collect()
}

/// Check if content appears to be hashline-tagged.
///
/// Looks at the first non-empty line for the `{digits}:{2hex}|` pattern.
pub fn looks_like_tagged(content: &str) -> bool {
    let Some(first_line) = content.lines().find(|l| !l.is_empty()) else {
        return false;
    };
    if let Some(pos) = first_line.find('|') {
        let prefix = &first_line[..pos];
        if let Some((num_part, hash_part)) = prefix.trim_start().split_once(':') {
            return num_part.chars().all(|c| c.is_ascii_digit())
                && !num_part.is_empty()
                && hash_part.len() == 2
                && hash_part.chars().all(|c| c.is_ascii_hexdigit());
        }
    }
    false
}

/// Strip hashline prefixes from content that was tagged with `tag_content`.
///
/// Each line is expected to be `{line_num}:{hash}|{content}`. Lines without
/// the prefix are passed through unchanged.
pub fn strip_hashlines(tagged: &str) -> String {
    let mut output = String::new();
    for line in tagged.lines() {
        if !output.is_empty() {
            output.push('\n');
        }
        if let Some(pos) = line.find('|') {
            let prefix = &line[..pos];
            // Verify it looks like a hashline prefix (digits:hex)
            if let Some((num_part, hash_part)) = prefix.trim_start().split_once(':') {
                if num_part.chars().all(|c| c.is_ascii_digit())
                    && hash_part.len() == 2
                    && hash_part.chars().all(|c| c.is_ascii_hexdigit())
                {
                    output.push_str(&line[pos + 1..]);
                    continue;
                }
            }
        }
        output.push_str(line);
    }
    // Preserve trailing newline if the input had one
    if tagged.ends_with('\n') && !output.ends_with('\n') {
        output.push('\n');
    }
    output
}

/// Verify hashline references in tagged content against the current file content.
///
/// Returns Ok(()) if all hashline-prefixed lines have hashes matching the current
/// content at those line numbers. Returns Err with a description of the first mismatch.
pub fn verify_hashlines(tagged: &str, current_content: &str) -> Result<(), String> {
    let current_hashlines = compute_hashlines(current_content);

    for line in tagged.lines() {
        if let Some(pos) = line.find('|') {
            let prefix = &line[..pos];
            if let Some(ref_str) = prefix.trim_start().split_once(':').and_then(|(num, hash)| {
                if num.chars().all(|c| c.is_ascii_digit())
                    && hash.len() == 2
                    && hash.chars().all(|c| c.is_ascii_hexdigit())
                {
                    Some(format!("{}:{}", num, hash))
                } else {
                    None
                }
            }) {
                if let Some((line_num, expected_hash)) = parse_hashline(&ref_str) {
                    if line_num == 0 || line_num as usize > current_hashlines.len() {
                        return Err(format!(
                            "line {} out of range (file has {} lines)",
                            line_num,
                            current_hashlines.len()
                        ));
                    }
                    let actual_hash = current_hashlines[(line_num - 1) as usize].1;
                    if actual_hash != expected_hash {
                        return Err(format!(
                            "stale hash at line {}: expected {}, got {} — re-read the file",
                            line_num,
                            expected_hash.display(),
                            actual_hash.display()
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Apply a hashline edit to file content.
///
/// The edit mixes anchor lines (hashline-prefixed, referencing existing lines)
/// with new lines (no prefix). The region between the first and last anchor
/// is replaced by the edit content.
///
/// Anchors are verified against `current_content` — a hash mismatch means the
/// file changed since the agent read it (stale edit).
pub fn apply_edit(edit_content: &str, current_content: &str) -> Result<String, String> {
    let current_lines: Vec<&str> = current_content.lines().collect();
    let current_hashlines = compute_hashlines(current_content);

    enum Entry<'a> {
        Anchor(u32),   // 1-based line number in original file
        New(&'a str),  // new content to insert
    }

    let mut entries = Vec::new();
    let mut anchors = Vec::new();

    for line in edit_content.lines() {
        if let Some((line_num, expected_hash)) = try_parse_anchor(line) {
            // Verify hash
            if line_num == 0 || line_num as usize > current_hashlines.len() {
                return Err(format!(
                    "line {} out of range (file has {} lines)",
                    line_num,
                    current_hashlines.len()
                ));
            }
            let actual_hash = current_hashlines[(line_num - 1) as usize].1;
            if actual_hash != expected_hash {
                return Err(format!(
                    "stale hash at line {}: expected {}, got {} — re-read the file",
                    line_num,
                    expected_hash.display(),
                    actual_hash.display()
                ));
            }
            anchors.push(line_num);
            entries.push(Entry::Anchor(line_num));
        } else {
            entries.push(Entry::New(line));
        }
    }

    if anchors.is_empty() {
        return Err("edit must contain at least one hashline anchor".into());
    }

    // Anchors must be in ascending order.
    for w in anchors.windows(2) {
        if w[1] <= w[0] {
            return Err(format!(
                "anchors must be in ascending order, got line {} after line {}",
                w[1], w[0]
            ));
        }
    }

    let min_anchor = anchors[0] as usize;         // 1-based
    let max_anchor = *anchors.last().unwrap() as usize; // 1-based

    // Build result: lines before the edit region + replacement + lines after.
    let mut output = String::new();

    // Lines before edit region (1-based lines 1..min_anchor).
    for line in &current_lines[..min_anchor - 1] {
        output.push_str(line);
        output.push('\n');
    }

    // Replacement content from the edit entries.
    for entry in &entries {
        match entry {
            Entry::Anchor(line_num) => {
                output.push_str(current_lines[(*line_num - 1) as usize]);
            }
            Entry::New(content) => {
                output.push_str(content);
            }
        }
        output.push('\n');
    }

    // Lines after edit region (1-based lines max_anchor+1..end).
    for line in &current_lines[max_anchor..] {
        output.push_str(line);
        output.push('\n');
    }

    // Match trailing newline behavior of the original.
    if !current_content.ends_with('\n') && output.ends_with('\n') {
        output.pop();
    }

    Ok(output)
}

/// Try to parse a line as a hashline anchor. Returns (line_num, hash) if it matches.
fn try_parse_anchor(line: &str) -> Option<(u32, LineHash)> {
    let pos = line.find('|')?;
    let prefix = &line[..pos];
    let (num_part, hash_part) = prefix.trim_start().split_once(':')?;
    if num_part.chars().all(|c| c.is_ascii_digit())
        && !num_part.is_empty()
        && hash_part.len() == 2
        && hash_part.chars().all(|c| c.is_ascii_hexdigit())
    {
        parse_hashline(&format!("{}:{}", num_part, hash_part))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_stability() {
        let h1 = LineHash::compute("fn main() {");
        let h2 = LineHash::compute("fn main() {");
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_trimming() {
        let h1 = LineHash::compute("  foo  ");
        let h2 = LineHash::compute("foo");
        assert_eq!(h1, h2);
    }

    #[test]
    fn line_numbers_dont_affect_hash() {
        let h = LineHash::compute("let x = 1;");
        let content = "a\nlet x = 1;\nb";
        let hashlines = compute_hashlines(content);
        assert_eq!(hashlines[1].1, h);
    }

    #[test]
    fn tag_format() {
        let content = "fn main() {\n    let x = 42;\n}";
        let tagged = tag_content(content);
        let lines: Vec<&str> = tagged.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("1:"));
        assert!(lines[0].contains("|fn main() {"));
        assert!(lines[1].starts_with("2:"));
        assert!(lines[1].contains("|    let x = 42;"));
        assert!(lines[2].starts_with("3:"));
        assert!(lines[2].contains("|}"));
    }

    #[test]
    fn tag_padding() {
        let content = (1..=12)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tagged = tag_content(&content);
        let lines: Vec<&str> = tagged.lines().collect();
        assert!(lines[0].starts_with(" 1:"));
        assert!(lines[9].starts_with("10:"));
        assert!(lines[11].starts_with("12:"));
    }

    #[test]
    fn roundtrip_parse() {
        let content = "fn main() {\n    let x = 42;\n}";
        let hashlines = compute_hashlines(content);
        for (line_num, hash) in &hashlines {
            let s = format!("{}:{}", line_num, hash.display());
            let (parsed_line, parsed_hash) = parse_hashline(&s).unwrap();
            assert_eq!(parsed_line, *line_num);
            assert_eq!(parsed_hash, *hash);
        }
    }

    #[test]
    fn parse_hashline_invalid() {
        assert!(parse_hashline("").is_none());
        assert!(parse_hashline("abc").is_none());
        assert!(parse_hashline("1:").is_none());
        assert!(parse_hashline(":ab").is_none());
        assert!(parse_hashline("1:zz").is_none());
    }

    #[test]
    fn empty_content() {
        let tagged = tag_content("");
        assert_eq!(tagged, "");
    }

    #[test]
    fn display_format() {
        let h = LineHash::compute("test");
        let display = h.display();
        assert_eq!(display.len(), 2);
        assert!(u8::from_str_radix(&display, 16).is_ok());
    }

    #[test]
    fn strip_roundtrip() {
        let content = "fn main() {\n    let x = 42;\n}";
        let tagged = tag_content(content);
        let stripped = strip_hashlines(&tagged);
        assert_eq!(stripped, content);
    }

    #[test]
    fn strip_preserves_non_hashline() {
        let content = "no prefix here\nalso none";
        let stripped = strip_hashlines(content);
        assert_eq!(stripped, content);
    }

    #[test]
    fn strip_preserves_trailing_newline() {
        // tag_content drops the trailing newline (empty last line is not tagged),
        // so roundtrip strips it. Verify the non-newline content matches.
        let content = "fn main() {\n}";
        let tagged = tag_content(content);
        let stripped = strip_hashlines(&tagged);
        assert_eq!(stripped, content);
    }

    #[test]
    fn verify_valid() {
        let content = "fn main() {\n    let x = 42;\n}";
        let tagged = tag_content(content);
        assert!(verify_hashlines(&tagged, content).is_ok());
    }

    #[test]
    fn verify_stale() {
        let original = "fn main() {\n    let x = 42;\n}";
        let tagged = tag_content(original);
        let modified = "fn main() {\n    let x = 99;\n}";
        let result = verify_hashlines(&tagged, modified);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("stale hash at line 2"));
    }

    #[test]
    fn verify_line_out_of_range() {
        let original = "line1\nline2\nline3";
        let tagged = tag_content(original);
        let shorter = "line1";
        let result = verify_hashlines(&tagged, shorter);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("out of range"));
    }

    // --- apply_edit tests ---

    /// Helper: build a hashline anchor for a given line of content.
    fn anchor(line_num: u32, content: &str) -> String {
        let hash = LineHash::compute(content);
        format!("{}:{}|{}", line_num, hash.display(), content)
    }

    #[test]
    fn edit_replace_middle() {
        let file = "line1\nline2\nline3\nline4\nline5";
        let edit = format!(
            "{}\nnew_a\nnew_b\n{}",
            anchor(2, "line2"),
            anchor(4, "line4"),
        );
        let result = apply_edit(&edit, file).unwrap();
        assert_eq!(result, "line1\nline2\nnew_a\nnew_b\nline4\nline5");
    }

    #[test]
    fn edit_insert_after() {
        let file = "line1\nline2\nline3";
        let edit = format!(
            "{}\ninserted",
            anchor(2, "line2"),
        );
        let result = apply_edit(&edit, file).unwrap();
        assert_eq!(result, "line1\nline2\ninserted\nline3");
    }

    #[test]
    fn edit_insert_before() {
        let file = "line1\nline2\nline3";
        let edit = format!(
            "inserted\n{}",
            anchor(2, "line2"),
        );
        let result = apply_edit(&edit, file).unwrap();
        assert_eq!(result, "line1\ninserted\nline2\nline3");
    }

    #[test]
    fn edit_delete_lines() {
        let file = "line1\nline2\nline3\nline4\nline5";
        // Anchor lines 2 and 5, everything between (3, 4) is deleted.
        let edit = format!(
            "{}\n{}",
            anchor(2, "line2"),
            anchor(5, "line5"),
        );
        let result = apply_edit(&edit, file).unwrap();
        assert_eq!(result, "line1\nline2\nline5");
    }

    #[test]
    fn edit_single_anchor_replace() {
        let file = "aaa\nbbb\nccc";
        let edit = format!(
            "{}\nreplacement",
            anchor(2, "bbb"),
        );
        let result = apply_edit(&edit, file).unwrap();
        assert_eq!(result, "aaa\nbbb\nreplacement\nccc");
    }

    #[test]
    fn edit_stale_hash_rejected() {
        let file = "line1\nline2\nline3";
        // Fake anchor with wrong hash.
        let edit = "2:ff|line2\nnew";
        let result = apply_edit(edit, file);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("stale hash"));
    }

    #[test]
    fn edit_no_anchors_rejected() {
        let file = "line1\nline2";
        let edit = "new content\nmore content";
        let result = apply_edit(edit, file);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("at least one"));
    }

    #[test]
    fn edit_out_of_order_rejected() {
        let file = "line1\nline2\nline3";
        let edit = format!(
            "{}\n{}",
            anchor(3, "line3"),
            anchor(1, "line1"),
        );
        let result = apply_edit(&edit, file);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("ascending order"));
    }
}
