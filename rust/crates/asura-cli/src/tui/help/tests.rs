use super::*;

fn build(guide: &str) -> Result<Index<'_>, ParseError> {
    Index::build(&[("guide", guide)])
}

fn ids(index: &Index, query: &str) -> Vec<String> {
    index
        .search(&Query::new(query))
        .rows
        .into_iter()
        .map(|row| index.entry(row).unwrap().id.clone())
        .collect()
}

const FIXED_GUIDE: &str = "# Test guide

## Queue panel

The queue panel lists waiting messages. It has more text.

## Shift keys

Shift moves focus.

## Planned queue (Planned)

Not built yet.
";
const FIXED_COMMANDS: &str = "# Commands

## Project commands

| Command | Action |
| --- | --- |
| `/project add PATH` | Add a folder |
| `/queue` | List the queue |
";
const FIXED_KEYS: &str = "# Keys

## Queue keys

| Key | Action |
| --- | --- |
| Enter | Send now |
| Shift+Up | Focus the log |

## Input box keys

| Key | Action |
| --- | --- |
| Shift+Up | Focus the queue |
| Enter | Send the message |
";
fn fixed() -> Index<'static> {
    Index::build(&[
        ("guide", FIXED_GUIDE),
        ("commands", FIXED_COMMANDS),
        ("keys", FIXED_KEYS),
    ])
    .unwrap()
}

#[test]
fn hu1_embedded_guide_builds_within_every_limit() {
    let index = index().expect("embedded guide parses");
    let entries = index.entries();
    assert!(!entries.is_empty() && entries.len() <= MAX_ENTRIES);
    let mut folded = 0;
    for entry in entries {
        assert!(entry.title.len() <= MAX_TITLE_BYTES, "{}", entry.id);
        assert!(entry.summary.len() <= MAX_SUMMARY_BYTES, "{}", entry.id);
        if entry.kind == Kind::Topic {
            assert!(entry.body.len() <= MAX_BODY_BYTES, "{}", entry.id);
            assert!(!entry.body.is_empty(), "{}", entry.id);
        }
        assert!(
            entry.body_folded.len() <= MAX_SEARCH_BODY_BYTES,
            "{}",
            entry.id
        );
        folded += entry.title_folded.len() + entry.labels_folded.len() + entry.body_folded.len();
    }
    assert!(folded <= MAX_FOLDED_BYTES);
    assert!(GUIDE.len() + COMMANDS.len() + KEYS.len() <= MAX_EMBEDDED_BYTES);
    let unique: HashSet<_> = entries.iter().map(|entry| &entry.id).collect();
    assert_eq!(unique.len(), entries.len());
    for entry in entries {
        let index_of = index.find(&entry.id).unwrap();
        assert_eq!(index.entry(index_of).unwrap().id, entry.id);
    }
    // One H1 topic per file, and every entry kind is present.
    for stem in ["guide", "commands", "keys"] {
        assert_eq!(
            entries
                .iter()
                .filter(|e| e.kind == Kind::Topic && e.id.starts_with(&format!("{stem}#")))
                .count()
                .min(1),
            1
        );
    }
    for kind in [Kind::Command, Kind::Key, Kind::Topic] {
        assert!(entries.iter().any(|e| e.kind == kind));
    }
}

#[test]
fn hu3_area_table_and_guide_rows_resolve() {
    let index = index().unwrap();
    for area in Area::ALL {
        for id in area.sections() {
            assert!(index.find(id).is_some(), "{id}");
        }
        let text = index.area_markdown(area).unwrap();
        assert!(!text.trim().is_empty());
    }
    for id in GUIDE_ROWS {
        assert!(index.title(id).is_some(), "{id}");
    }
    // Rows open their own text first, then their section.
    let queue_enter = index.find("key:queue-keys:Enter").unwrap();
    let text = index.markdown(queue_enter).unwrap();
    assert!(text.starts_with("**Queue: Enter**"));
    assert!(text.contains("## Queue keys"));
    let command = index.find("command:/project add PATH").unwrap();
    assert!(
        index
            .markdown(command)
            .unwrap()
            .contains("## Project commands")
    );
    // The text copied for an entry is its own source section only.
    let topic = index.find("guide#use-the-queue").unwrap();
    let text = index.markdown(topic).unwrap();
    assert!(text.starts_with("## Use the queue"));
    assert!(!text.contains("## Steer or queue"));
}

#[test]
fn hu3_index_ids_follow_the_documented_forms() {
    let index = fixed();
    for id in [
        "guide#queue-panel",
        "guide#planned-queue-planned",
        "command:/project add PATH",
        "key:queue-keys:Shift+Up",
        "key:input-box-keys:Shift+Up",
    ] {
        assert!(index.find(id).is_some(), "{id}");
    }
    let planned = index.entry(index.find("guide#planned-queue-planned").unwrap());
    let planned = planned.unwrap();
    assert!(planned.planned);
    assert_eq!(planned.title, "Planned queue");
    assert_eq!(
        index
            .entry(index.find("key:queue-keys:Enter").unwrap())
            .unwrap()
            .title,
        "Queue: Enter"
    );
}

#[test]
fn hu5_format_contract_violations_return_typed_errors() {
    assert_eq!(
        build("## No title\n\ntext\n").unwrap_err(),
        ParseError::MissingTitle("guide".into())
    );
    assert_eq!(
        build("text first\n\n# Late title\n").unwrap_err(),
        ParseError::MissingTitle("guide".into())
    );
    assert_eq!(
        build("").unwrap_err(),
        ParseError::MissingTitle("guide".into())
    );
    assert_eq!(
        build("# One\n\n# Two\n").unwrap_err(),
        ParseError::MultipleTitles("guide".into())
    );
    assert!(matches!(
        build("# T\n\n## S\n\n| Command | Meaning |\n| --- | --- |\n| `/a` | b |\n"),
        Err(ParseError::BadTableHeader { .. })
    ));
    assert!(matches!(
        build("# T\n\n## S\n\n| Key | Reason |\n| --- | --- |\n| Up | b |\n"),
        Err(ParseError::BadTableHeader { .. })
    ));
    for cell in ["/plain", "`project`", "`/a` and `/b`", "see `/a`"] {
        let source =
            format!("# T\n\n## S\n\n| Command | Action |\n| --- | --- |\n| {cell} | x |\n");
        assert!(
            matches!(build(&source), Err(ParseError::BadCommandCell { .. })),
            "{cell}"
        );
    }
    assert_eq!(
        build("# T\n\n## Same\n\na\n\n## Same\n\nb\n").unwrap_err(),
        ParseError::DuplicateId("guide#same".into())
    );
    assert_eq!(
        build("# T\n\n## S\n\n| Command | Action |\n| --- | --- |\n| `/a` | x |\n| `/a` | y |\n")
            .unwrap_err(),
        ParseError::DuplicateId("command:/a".into())
    );
    let big = format!("# T\n\n## Big\n\n{}\n", "word ".repeat(MAX_BODY_BYTES / 4));
    assert_eq!(
        build(&big).unwrap_err(),
        ParseError::EntryTooLarge {
            id: "guide#big".into(),
            field: "body"
        }
    );
    let long = format!("# T\n\n## {}\n\nx\n", "t".repeat(MAX_TITLE_BYTES + 1));
    assert!(matches!(
        build(&long),
        Err(ParseError::EntryTooLarge { field: "title", .. })
    ));
    assert!(matches!(
        build("# T\n\nSee [x](missing.md) now.\n"),
        Err(ParseError::UnresolvedLink { .. })
    ));
    assert!(matches!(
        build("# T\n\nSee [x](#nope) now.\n"),
        Err(ParseError::UnresolvedLink { .. })
    ));
    assert!(matches!(
        build("# T\n\nSee [x](../README.md) now.\n"),
        Err(ParseError::UnresolvedLink { .. })
    ));
    // Valid links, including a whole-file link, resolve; external ones are not checked.
    assert!(
        build("# T\n\n## Same\n\nSee [a](#same), [b](guide.md), [c](https://example.com).\n")
            .is_ok()
    );
}

#[test]
fn hu18_fixed_index_ranks_titles_labels_and_kinds() {
    let index = fixed();
    assert_eq!(
        ids(&index, "shift up"),
        [
            "key:queue-keys:Shift+Up",
            "key:input-box-keys:Shift+Up",
            "keys#queue-keys",
            "keys#input-box-keys"
        ]
    );
    assert_eq!(
        ids(&index, "/proj"),
        ["command:/project add PATH", "commands#project-commands"]
    );
    assert_eq!(
        ids(&index, "ENTER"),
        [
            "key:queue-keys:Enter",
            "key:input-box-keys:Enter",
            "keys#queue-keys",
            "keys#input-box-keys"
        ]
    );
    assert_eq!(
        ids(&index, "queue enter"),
        [
            "key:queue-keys:Enter",
            "keys#queue-keys",
            "keys#input-box-keys"
        ]
    );
    // A title that starts with the query adds 4; commands outrank topics on a
    // tie; a planned topic sorts after implemented entries with its score; a
    // body-only match comes last.
    assert_eq!(
        ids(&index, "queue"),
        [
            "key:queue-keys:Enter",
            "key:queue-keys:Shift+Up",
            "guide#queue-panel",
            "keys#queue-keys",
            "command:/queue",
            "guide#planned-queue-planned",
            "key:input-box-keys:Shift+Up",
            "commands#project-commands",
            "keys#input-box-keys",
        ]
    );
    let none = index.search(&Query::new("zzz"));
    assert!(none.rows.is_empty() && none.total == 0);
    // The kind label and area name are searchable.
    assert!(ids(&index, "command").contains(&"command:/queue".to_string()));
    assert!(ids(&index, "planned").contains(&"guide#planned-queue-planned".to_string()));
}

#[test]
fn hu18_fold_replaces_separators_and_lowercases() {
    assert_eq!(fold("Shift+Up"), "shift up");
    assert_eq!(fold("Alt+B, Alt+F · Ctrl—X"), "alt b alt f ctrl x");
    assert_eq!(fold("  İ\tTAB \n"), fold("İ tab"));
}

#[test]
fn hu19_long_query_uses_first_256_bytes_and_eight_terms() {
    let words: Vec<String> = (0..12).map(|n| format!("w{n}")).collect();
    let mut text = words.join(" ");
    text.push_str(&" ".repeat(255 - text.len()));
    text.push('é');
    text.push_str(" tail more words after the cut");
    assert!(text.len() > 256 && !text.is_char_boundary(256));
    let query = Query::new(&text);
    assert_eq!(query.terms().len(), MAX_TERMS);
    assert_eq!(query.terms(), &words[..8]);
    assert!(!query.folded.contains('é'));
    // A term that straddles byte 256 is cut at the previous character boundary.
    let straddle = format!("{}éyy", "x".repeat(255));
    let query = Query::new(&straddle);
    assert_eq!(query.terms(), ["x".repeat(255)]);
    // Duplicates collapse and blank text has no terms.
    assert_eq!(Query::new("a A a").terms(), ["a"]);
    assert!(Query::new(" \t+·—, ").is_blank());
}

#[test]
fn hu22_entry_cap_and_bounded_search_work() {
    let guide = |count: usize| {
        let mut source = String::from("# Root\n\n");
        for n in 1..count {
            source.push_str(&format!("## Item {n} common\n\nbody {n} common\n\n"));
        }
        source
    };
    let source = guide(MAX_ENTRIES);
    let index = build(&source).unwrap();
    assert_eq!(index.entries().len(), MAX_ENTRIES);
    let source = guide(MAX_ENTRIES + 1);
    assert_eq!(build(&source).unwrap_err(), ParseError::TooManyEntries);
    for n in 0..1000 {
        let query = Query::new(&format!("common item {} body", n % 7));
        let results = index.search(&query);
        assert!(results.rows.len() <= MAX_RESULTS);
        assert!(results.total <= MAX_ENTRIES);
        assert!(results.work <= MAX_ENTRIES * query.terms().len());
    }
    let all = index.search(&Query::new("common"));
    assert_eq!(all.rows.len(), MAX_RESULTS);
    assert_eq!(all.total, MAX_ENTRIES - 1);
    // Eight terms is the per-entry bound.
    let eight = index.search(&Query::new("a b c d e f g h i j"));
    assert!(eight.work <= MAX_ENTRIES * MAX_TERMS);
}

#[test]
fn folded_text_over_the_total_cap_is_rejected() {
    let mut source = String::from("# Root\n\n");
    for n in 0..120 {
        source.push_str(&format!("## Item {n}\n\n"));
        source.push_str(&format!("w{n} ").repeat(1000));
        source.push_str("\n\n");
    }
    assert_eq!(build(&source).unwrap_err(), ParseError::FoldedTextTooLarge);
}

#[test]
fn table_rows_list_commands_then_keys_in_guide_order() {
    let index = fixed();
    let titles: Vec<_> = index.table_rows().into_iter().map(|row| row.0).collect();
    assert_eq!(
        titles,
        [
            "/project add PATH",
            "/queue",
            "Queue: Enter",
            "Queue: Shift+Up",
            "Input box: Shift+Up",
            "Input box: Enter"
        ]
    );
}
