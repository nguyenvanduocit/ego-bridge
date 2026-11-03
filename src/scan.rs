//! Find the absolute paths a script names, so the client can ship the files
//! the script will read and create the directories it will write into.

/// Absolute POSIX paths that appear as complete string literals in `script`.
///
/// Recognises `"..."`, `'...'` and template literals without `${}`. A
/// `file://` URL literal is returned as its path. Order follows first
/// appearance; duplicates are dropped.
pub fn absolute_path_literals(script: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut chars = script.chars();
    while let Some(c) = chars.next() {
        if c != '"' && c != '\'' && c != '`' {
            continue;
        }
        let quote = c;
        let mut literal = String::new();
        let mut closed = false;
        let mut escaped = false;
        for ch in chars.by_ref() {
            if escaped {
                literal.push(ch);
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                '\n' if quote != '`' => break,
                ch if ch == quote => {
                    closed = true;
                    break;
                }
                ch => literal.push(ch),
            }
        }
        if !closed || (quote == '`' && literal.contains("${")) {
            continue;
        }
        if let Some(path) = as_absolute_path(&literal)
            && !found.iter().any(|existing| existing == &path)
        {
            found.push(path);
        }
    }
    found
}

fn as_absolute_path(literal: &str) -> Option<String> {
    let path = literal.strip_prefix("file://").unwrap_or(literal);
    let looks_absolute = path.starts_with('/') && !path.starts_with("//") && path.len() > 1;
    let plausible = !path.contains(['\0', '\n', '*', '?', '<', '>', '|']);
    (looks_absolute && plausible).then(|| path.to_owned())
}

#[cfg(test)]
mod tests {
    use super::absolute_path_literals;

    #[test]
    fn finds_paths_in_every_quote_style() {
        let script = r#"
            await page.setInputFiles("input", ["/home/pod/a.pdf", '/home/pod/b.png']);
            await page.screenshot({ path: `/tmp/shot.png` });
        "#;
        assert_eq!(
            absolute_path_literals(script),
            vec!["/home/pod/a.pdf", "/home/pod/b.png", "/tmp/shot.png"]
        );
    }

    #[test]
    fn ignores_relative_urls_selectors_and_interpolated_templates() {
        let script = r#"
            await page.goto("https://example.com/a/b");
            await page.fetch("/api/items");
            await page.click("a[href='/login']");
            const p = `/tmp/${name}.png`;
            const c = "// comment-like";
        "#;
        // "/api/items" is indistinguishable from a path and is returned; the
        // client only acts on it when such a file or directory exists locally.
        assert_eq!(absolute_path_literals(script), vec!["/api/items"]);
    }

    #[test]
    fn unwraps_file_urls_and_deduplicates() {
        let script = r#"open("file:///tmp/a.txt"); open("/tmp/a.txt");"#;
        assert_eq!(absolute_path_literals(script), vec!["/tmp/a.txt"]);
    }

    #[test]
    fn keeps_escaped_quotes_inside_literals() {
        let script = r#"write("/tmp/it\"s.txt")"#;
        assert_eq!(absolute_path_literals(script), vec!["/tmp/it\"s.txt"]);
    }
}
