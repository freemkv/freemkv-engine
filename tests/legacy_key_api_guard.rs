//! The structural key guard (keys-upfront design v3.4 §2.2, EK9 structural half, KU-X1).
//!
//! §2.2: "One test per repo; any match outside the allow-path fails." Every Rust file under
//! `src/` (`#[cfg(test)]` included) and `tests/` is scanned, comments skipped (EK9: "The
//! structural half skips comments."), for the engine's banned list: the libfreemkv doors
//! plus `DecryptingSectorSource::new(`, `DiscStream::new(`, `KeyFetch`, `key_fetch(`, the
//! legacy mapfile key and raw-VID line prefixes, and `set_vid(`.
//!
//! The allow-path is `recovery/mapfile.rs` fn `parse_legacy_key_lines`, the read-only
//! legacy-line parser. The tests that prove those mapfile lines absent or scrubbed (EK1,
//! EK2, `the_mapfile_header_*`) may name the two line prefixes, and nothing else. The
//! sanctioned test helper `libfreemkv::test_util::decrypt_unit` is not the library door.
//! Per spec; do not change without a spec citation proving otherwise.

use std::path::{Path, PathBuf};

// The mapfile line prefixes, assembled so this file never spells them.
const UK: &str = concat!("freemkv", "-uk");
const VID: &str = concat!("freemkv", "-vid:");

/// §2.2's engine list: "all of the above" (the libfreemkv row) plus the engine additions.
fn banned() -> Vec<&'static str> {
    vec![
        ".get_unit_keys(",
        ".get_fmts_indexes(",
        ".resolve_unit_keys(",
        "decrypt_with(",
        "AacsKeyMap::from_ranges",
        "with_key_map(",
        "set_key_map(",
        "decrypt_unit(",
        "DecryptingSectorSource::new(",
        "DiscStream::new(",
        "KeyFetch",
        "key_fetch(",
        UK,
        VID,
        "set_vid(",
    ]
}

/// `src` with every comment blanked (newlines kept), and a copy with string and char
/// literal contents blanked too; both keep `src`'s char positions.
fn lex(src: &str) -> (Vec<char>, Vec<char>) {
    let s: Vec<char> = src.chars().collect();
    let mut code = s.clone();
    let mut bare = s.clone();
    let blank = |v: &mut Vec<char>, from: usize, to: usize| {
        for c in &mut v[from..to] {
            if *c != '\n' {
                *c = ' ';
            }
        }
    };
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let (n, mut i) = (s.len(), 0);
    while i < n {
        let at = |k: usize| s.get(k).copied().unwrap_or('\0');
        if at(i) == '/' && at(i + 1) == '/' {
            let end = (i..n).find(|&k| s[k] == '\n').unwrap_or(n);
            blank(&mut code, i, end);
            blank(&mut bare, i, end);
            i = end;
        } else if at(i) == '/' && at(i + 1) == '*' {
            let (mut depth, mut k) = (1, i + 2);
            while k < n && depth > 0 {
                if at(k) == '/' && at(k + 1) == '*' {
                    depth += 1;
                    k += 2;
                } else if at(k) == '*' && at(k + 1) == '/' {
                    depth -= 1;
                    k += 2;
                } else {
                    k += 1;
                }
            }
            blank(&mut code, i, k);
            blank(&mut bare, i, k);
            i = k;
        } else if at(i) == 'r'
            && (i == 0 || !word(s[i - 1]) || (s[i - 1] == 'b' && (i < 2 || !word(s[i - 2]))))
            && {
                let h = (i + 1..n).take_while(|&k| s[k] == '#').count();
                at(i + 1 + h) == '"'
            }
        {
            let h = (i + 1..n).take_while(|&k| s[k] == '#').count();
            let open = i + 2 + h;
            let close: String = std::iter::once('"')
                .chain(std::iter::repeat_n('#', h))
                .collect();
            let close: Vec<char> = close.chars().collect();
            let end = (open..n).find(|&k| s[k..].starts_with(&close)).unwrap_or(n);
            blank(&mut bare, open, end);
            i = (end + close.len()).min(n);
        } else if at(i) == '"' {
            let mut k = i + 1;
            while k < n && s[k] != '"' {
                k += if s[k] == '\\' { 2 } else { 1 };
            }
            blank(&mut bare, i + 1, k.min(n));
            i = k + 1;
        } else if at(i) == '\'' && (at(i + 1) == '\\' || at(i + 2) == '\'') {
            let end = (i + 2..n).find(|&k| s[k] == '\'').unwrap_or(n);
            blank(&mut bare, i + 1, end);
            i = end + 1;
        } else {
            i += 1;
        }
    }
    (code, bare)
}

/// Char spans of the bodies of the `fn`s in `bare` whose name satisfies `pick`.
fn fn_spans(bare: &[char], pick: impl Fn(&str) -> bool) -> Vec<(usize, usize)> {
    let text: String = bare.iter().collect();
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let to_char = |byte: usize| chars.partition_point(|&(b, _)| b < byte);
    let mut spans = Vec::new();
    for (at, _) in text.match_indices("fn ") {
        let name: String = text[at + 3..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if name.is_empty() || !pick(&name) {
            continue;
        }
        let start = to_char(at);
        let Some(open) = (start..bare.len()).find(|&k| bare[k] == '{') else {
            continue;
        };
        let mut depth = 0;
        for (k, &c) in bare.iter().enumerate().skip(open) {
            depth += (c == '{') as i32 - (c == '}') as i32;
            if depth == 0 {
                spans.push((start, k + 1));
                break;
            }
        }
    }
    spans
}

/// `(line, token)` of every banned match in `src` (file `rel`) outside the allow-path.
fn hits(rel: &str, src: &str) -> Vec<(usize, &'static str)> {
    let (code, bare) = lex(src);
    let allow = fn_spans(&bare, |n| {
        rel == "src/recovery/mapfile.rs" && n == "parse_legacy_key_lines"
    });
    let line_tests = fn_spans(&bare, |n| match rel {
        "src/recovery/mapfile.rs" => [
            "mapfile_never_writes_key_or_raw_vid_lines",
            "legacy_lines_become_fingerprints_on_first_write",
        ]
        .contains(&n),
        "tests/recovery_copy_dispatch.rs" => n.starts_with("the_mapfile_header_"),
        _ => false,
    });
    let inside = |spans: &[(usize, usize)], k: usize| spans.iter().any(|&(a, b)| a <= k && k < b);
    let mut out = Vec::new();
    for tok in banned() {
        let t: Vec<char> = tok.chars().collect();
        for k in 0..code.len().saturating_sub(t.len() - 1) {
            if code[k..k + t.len()] != t[..] || inside(&allow, k) {
                continue;
            }
            if (tok == UK || tok == VID) && inside(&line_tests, k) {
                continue;
            }
            let helper: Vec<char> = "test_util::".chars().collect();
            if tok == "decrypt_unit("
                && k >= helper.len()
                && code[k - helper.len()..k] == helper[..]
            {
                continue;
            }
            out.push((code[..k].iter().filter(|&&c| c == '\n').count() + 1, tok));
        }
    }
    out.sort();
    out
}

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// EK9 structural half (KU §2.2, KU-X1): no legacy key API in `src/` or `tests/` outside
/// `recovery/mapfile.rs` fn `parse_legacy_key_lines`.
#[test]
fn no_legacy_key_api_outside_parse_legacy_key_lines() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rs_files(&root.join("src"), &mut files);
    rs_files(&root.join("tests"), &mut files);
    let mapfile = std::fs::read_to_string(root.join("src/recovery/mapfile.rs")).unwrap();
    let (_, bare) = lex(&mapfile);
    assert_eq!(
        fn_spans(&bare, |n| n == "parse_legacy_key_lines").len(),
        1,
        "the allow-path fn must exist"
    );
    let mut found = Vec::new();
    for f in &files {
        let rel = f
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if rel == "tests/legacy_key_api_guard.rs" {
            continue;
        }
        let src = std::fs::read_to_string(f).expect("read source");
        for (line, tok) in hits(&rel, &src) {
            found.push(format!("{rel}:{line}: {tok}"));
        }
    }
    assert!(
        found.is_empty(),
        "legacy key API outside the allow-path:\n{}",
        found.join("\n")
    );
}

/// Self-test: comments are skipped, code and string literals are not, and each allowance
/// holds only where it is granted.
#[test]
fn the_structural_guard_skips_comments_and_keeps_its_allow_path() {
    let code = "fn f() { m.set_vid(v); }\n";
    assert_eq!(hits("src/x.rs", code), [(1, "set_vid(")]);
    let commented =
        "// m.set_vid(v)\n/* KeyFetch /* nested */ key_fetch( */\n/// DiscStream::new(\n";
    assert!(
        hits("src/x.rs", commented).is_empty(),
        "comments are skipped"
    );
    let url = "let u = \"http://x\"; let k: KeyFetch = f;\n";
    assert_eq!(
        hits("src/x.rs", url),
        [(1, "KeyFetch")],
        "`//` inside a string is data"
    );
    let literal = format!("let s = \"# {UK}: 1:00\";\n");
    assert_eq!(
        hits("src/x.rs", &literal),
        [(1, UK)],
        "string literals are scanned"
    );
    let raw = format!("let s = r#\"{VID} // \"#; x.decrypt_with(k);\n");
    assert_eq!(
        hits("src/x.rs", &raw).len(),
        2,
        "raw strings are data, not comments"
    );

    let parser = format!(
        "fn parse_legacy_key_lines(c: &str) {{\n    c.strip_prefix(\"{VID}\");\n}}\n\
         fn other() {{ c.strip_prefix(\"{UK}:\"); }}\n"
    );
    assert!(
        hits("src/recovery/mapfile.rs", &parser).len() == 1,
        "only the named fn"
    );
    assert_eq!(
        hits("src/other.rs", &parser).len(),
        2,
        "only in recovery/mapfile.rs"
    );

    let scrub = format!("fn the_mapfile_header_x() {{ assert!(!t.contains(\"{VID}\")); }}\n");
    assert!(hits("tests/recovery_copy_dispatch.rs", &scrub).is_empty());
    let api = "fn the_mapfile_header_x() { m.set_vid(v); }\n";
    assert_eq!(
        hits("tests/recovery_copy_dispatch.rs", api).len(),
        1,
        "prefixes only"
    );

    let helper =
        "libfreemkv::test_util::decrypt_unit(&mut u, k); content::decrypt_unit(&mut u, k);";
    assert_eq!(hits("src/x.rs", helper).len(), 1, "only the library door");
    let lifetime = "fn g<'a>(x: &'a str) -> char { let c = '\"'; x.set_key_map(m); c }\n";
    assert_eq!(hits("src/x.rs", lifetime), [(1, "set_key_map(")]);
}
