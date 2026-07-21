use super::*;

// --- xxHash32 / tag ---------------------------------------------------------

#[test]
fn xxh32_reference_vectors() {
    // Official empty-string vector.
    assert_eq!(xxh32(b"", 0), 0x02CC_5D05);
    // Exercises the 16-byte stripe loop (40 bytes); cross-checked against an
    // independent implementation.
    let long = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no\np\nq\nr\ns\nt\n";
    assert_eq!(xxh32(long.as_bytes(), 0), 0x6B6E_D5C9);
}

#[test]
fn compute_tag_golden() {
    assert_eq!(compute_tag(""), "5D05");
    assert_eq!(compute_tag("hello\n"), "5BF9");
    assert_eq!(compute_tag("fn main() {\n    println!(\"hi\");\n}\n"), "A612");
    assert_eq!(compute_tag("line one\nline two\nline three\n"), "2172");
    assert_eq!(
        compute_tag("a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no\np\nq\nr\ns\nt\n"),
        "D5C9"
    );
}

#[test]
fn tag_ignores_trailing_whitespace_and_cr() {
    let base = "hello\nworld\n";
    assert_eq!(compute_tag(base), compute_tag("hello   \nworld\t\n"));
    // CRLF line endings hash the same as LF.
    assert_eq!(compute_tag(base), compute_tag("hello\r\nworld\r\n"));
}

// --- parser -----------------------------------------------------------------

#[test]
fn parse_header_and_uppercases_tag() {
    let secs = parse("[src/main.rs#a1b2]\nDEL 3\n").unwrap();
    assert_eq!(secs.len(), 1);
    assert_eq!(secs[0].path, "src/main.rs");
    assert_eq!(secs[0].tag, "A1B2");
    assert_eq!(secs[0].ops, vec![Op::Del { start: 3, end: 3 }]);
}

#[test]
fn parse_all_op_types_with_bodies() {
    let patch = "\
[f#1234]
SWAP 2.=4:
+new2
+new3
SWAP.BLK 10:
+block
DEL 6
DEL 8.=9
DEL.BLK 12
INS.PRE 1:
+pre
INS.POST 5:
+post
INS.HEAD:
+head
INS.TAIL:
+tail
INS.BLK.POST 20:
+afterblock
";
    let ops = parse(patch).unwrap().pop().unwrap().ops;
    assert_eq!(
        ops,
        vec![
            Op::Swap { start: 2, end: 4, body: vec!["new2".into(), "new3".into()] },
            Op::SwapBlk { start: 10, body: vec!["block".into()] },
            Op::Del { start: 6, end: 6 },
            Op::Del { start: 8, end: 9 },
            Op::DelBlk { start: 12 },
            Op::InsPre { line: 1, body: vec!["pre".into()] },
            Op::InsPost { line: 5, body: vec!["post".into()] },
            Op::InsHead { body: vec!["head".into()] },
            Op::InsTail { body: vec!["tail".into()] },
            Op::InsBlkPost { line: 20, body: vec!["afterblock".into()] },
        ]
    );
}

#[test]
fn parse_multi_file_sections() {
    let patch = "[a.rs#0001]\nDEL 1\n[b.rs#0002]\nDEL 2\nINS.TAIL:\n+x\n";
    let secs = parse(patch).unwrap();
    assert_eq!(secs.len(), 2);
    assert_eq!(secs[0].path, "a.rs");
    assert_eq!(secs[0].ops, vec![Op::Del { start: 1, end: 1 }]);
    assert_eq!(secs[1].path, "b.rs");
    assert_eq!(
        secs[1].ops,
        vec![Op::Del { start: 2, end: 2 }, Op::InsTail { body: vec!["x".into()] }]
    );
}

#[test]
fn parse_empty_body_line_is_blank_line() {
    // A bare `+` inserts an empty line.
    let ops = parse("[f#1234]\nINS.HEAD:\n+\n+x\n").unwrap().pop().unwrap().ops;
    assert_eq!(ops, vec![Op::InsHead { body: vec!["".into(), "x".into()] }]);
}

#[test]
fn parse_errors() {
    assert!(parse("DEL 1\n").is_err(), "op before header");
    assert!(parse("[f#1]\n+body\n").is_err(), "body before op");
    assert!(parse("[f#1]\nDEL 1\n+oops\n").is_err(), "DEL takes no body");
    assert!(parse("[f#1]\nWAT 1:\n").is_err(), "unknown op");
    assert!(parse("[f#1]\nDEL 0\n").is_err(), "zero line number");
    assert!(parse("[f#1]\nSWAP 5.=2:\n").is_err(), "reversed range");
    assert!(parse("[f#1]\nSWAP 2\n").is_err(), "SWAP needs colon");
    assert!(parse("no sections here\n").is_err(), "no header");
}

// --- apply: each op ---------------------------------------------------------

fn sec(ops: Vec<Op>) -> FileSection {
    FileSection { path: "f".into(), tag: "0000".into(), ops }
}

#[test]
fn apply_swap() {
    let out = apply(
        "a\nb\nc\n",
        &sec(vec![Op::Swap { start: 2, end: 2, body: vec!["B1".into(), "B2".into()] }]),
    )
    .unwrap();
    assert_eq!(out, "a\nB1\nB2\nc\n");
}

#[test]
fn apply_swap_range() {
    let out = apply(
        "a\nb\nc\nd\n",
        &sec(vec![Op::Swap { start: 2, end: 3, body: vec!["X".into()] }]),
    )
    .unwrap();
    assert_eq!(out, "a\nX\nd\n");
}

#[test]
fn apply_del_single_and_range() {
    assert_eq!(apply("a\nb\nc\n", &sec(vec![Op::Del { start: 2, end: 2 }])).unwrap(), "a\nc\n");
    assert_eq!(
        apply("a\nb\nc\nd\n", &sec(vec![Op::Del { start: 2, end: 3 }])).unwrap(),
        "a\nd\n"
    );
}

#[test]
fn apply_inserts() {
    assert_eq!(
        apply("a\nb\n", &sec(vec![Op::InsHead { body: vec!["H".into()] }])).unwrap(),
        "H\na\nb\n"
    );
    assert_eq!(
        apply("a\nb\n", &sec(vec![Op::InsTail { body: vec!["T".into()] }])).unwrap(),
        "a\nb\nT\n"
    );
    assert_eq!(
        apply("a\nb\n", &sec(vec![Op::InsPre { line: 2, body: vec!["P".into()] }])).unwrap(),
        "a\nP\nb\n"
    );
    assert_eq!(
        apply("a\nb\n", &sec(vec![Op::InsPost { line: 1, body: vec!["P".into()] }])).unwrap(),
        "a\nP\nb\n"
    );
}

#[test]
fn apply_block_ops() {
    let src = "def f():\n    a = 1\n    b = 2\nc = 3\n";
    // block starting at line 1 spans lines 1..=3.
    assert_eq!(
        apply(src, &sec(vec![Op::DelBlk { start: 1 }])).unwrap(),
        "c = 3\n"
    );
    assert_eq!(
        apply(src, &sec(vec![Op::SwapBlk { start: 1, body: vec!["def g(): pass".into()] }]))
            .unwrap(),
        "def g(): pass\nc = 3\n"
    );
    assert_eq!(
        apply(src, &sec(vec![Op::InsBlkPost { line: 1, body: vec!["# end".into()] }])).unwrap(),
        "def f():\n    a = 1\n    b = 2\n# end\nc = 3\n"
    );
}

#[test]
fn apply_block_includes_interior_blank_excludes_trailing_blank() {
    let src = "def f():\n    a = 1\n\n    b = 2\n\nc = 3\n";
    // interior blank (line 3) is part of the block; trailing blank (line 5) is not.
    assert_eq!(apply(src, &sec(vec![Op::DelBlk { start: 1 }])).unwrap(), "\nc = 3\n");
}

#[test]
fn apply_multiple_ops_original_coords() {
    // All ops anchor to ORIGINAL line numbers.
    let out = apply(
        "1\n2\n3\n4\n5\n",
        &sec(vec![
            Op::Del { start: 2, end: 2 },
            Op::Swap { start: 4, end: 4, body: vec!["FOUR".into()] },
            Op::InsHead { body: vec!["ZERO".into()] },
        ]),
    )
    .unwrap();
    assert_eq!(out, "ZERO\n1\n3\nFOUR\n5\n");
}

#[test]
fn apply_preserves_missing_trailing_newline() {
    assert_eq!(
        apply("a\nb\nc", &sec(vec![Op::Del { start: 2, end: 2 }])).unwrap(),
        "a\nc"
    );
}

// --- apply: rejection -------------------------------------------------------

#[test]
fn reject_overlapping_cuts() {
    let err = apply(
        "1\n2\n3\n4\n5\n",
        &sec(vec![
            Op::Swap { start: 2, end: 4, body: vec!["X".into()] },
            Op::Del { start: 3, end: 3 },
        ]),
    )
    .unwrap_err();
    assert!(err.to_string().contains("overlapping"), "{err}");
}

#[test]
fn reject_insertion_inside_cut() {
    let err = apply(
        "1\n2\n3\n4\n5\n",
        &sec(vec![
            Op::Del { start: 2, end: 4 },
            Op::InsPost { line: 3, body: vec!["X".into()] },
        ]),
    )
    .unwrap_err();
    assert!(err.to_string().contains("inside deleted range"), "{err}");
}

#[test]
fn reject_duplicate_insertion_point() {
    let err = apply(
        "1\n2\n3\n",
        &sec(vec![
            Op::InsPost { line: 1, body: vec!["A".into()] },
            Op::InsPre { line: 2, body: vec!["B".into()] },
        ]),
    )
    .unwrap_err();
    assert!(err.to_string().contains("same insertion point"), "{err}");
}

#[test]
fn reject_out_of_range() {
    let err = apply("a\nb\n", &sec(vec![Op::Del { start: 9, end: 9 }])).unwrap_err();
    assert!(err.to_string().contains("out of range"), "{err}");
}

// --- apply_section: tag check + stale recovery ------------------------------

fn ten_lines() -> String {
    (1..=10).map(|i| format!("line{i}\n")).collect()
}

#[test]
fn apply_section_tag_match() {
    let live = ten_lines();
    let section = FileSection {
        path: "f".into(),
        tag: compute_tag(&live),
        ops: vec![Op::Swap { start: 5, end: 5, body: vec!["LINE5".into()] }],
    };
    let applied = apply_section(&section, &live, None).unwrap();
    assert!(!applied.recovered);
    assert!(applied.content.contains("LINE5\n"));
    assert!(!applied.content.contains("line5\n"));
}

#[test]
fn apply_section_recovers_from_snapshot_when_edit_region_intact() {
    let snap = ten_lines();
    let section = FileSection {
        path: "f".into(),
        tag: compute_tag(&snap), // patch authored against the snapshot
        ops: vec![Op::Swap { start: 5, end: 5, body: vec!["LINE5-EDIT".into()] }],
    };
    // Live drifted far from the edit (line 9 changed) => tag differs, region intact.
    let live = snap.replace("line9\n", "line9-changed\n");
    assert_ne!(compute_tag(&live), section.tag);

    let applied = apply_section(&section, &live, Some(&snap)).unwrap();
    assert!(applied.recovered);
    assert!(applied.content.contains("LINE5-EDIT\n"));
    assert!(applied.content.contains("line9-changed\n"), "unrelated live edit preserved");
}

#[test]
fn apply_section_recovery_fails_when_region_changed() {
    let snap = ten_lines();
    let section = FileSection {
        path: "f".into(),
        tag: compute_tag(&snap),
        ops: vec![Op::Swap { start: 5, end: 5, body: vec!["LINE5-EDIT".into()] }],
    };
    // Live changed the very region being edited => zero-fuzz recovery must reject.
    let live = snap.replace("line5\n", "line5-conflict\n");
    let err = apply_section(&section, &live, Some(&snap)).unwrap_err();
    assert!(err.to_string().contains("recovery failed"), "{err}");
}

#[test]
fn apply_section_no_snapshot_errors_with_reread_hint() {
    let snap = ten_lines();
    let section = FileSection {
        path: "f".into(),
        tag: compute_tag(&snap),
        ops: vec![Op::Del { start: 5, end: 5 }],
    };
    let live = snap.replace("line1\n", "line1-changed\n");
    let err = apply_section(&section, &live, None).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("Re-read"), "{msg}");
}

// --- SnapshotStore ----------------------------------------------------------

#[test]
fn snapshot_store_record_and_lookup() {
    let store = SnapshotStore::new();
    let text = "hello\nworld\n";
    let tag = compute_tag(text);
    store.record("/p/a.rs", text);
    assert_eq!(store.lookup("/p/a.rs", &tag).as_deref(), Some(text));
    assert_eq!(store.lookup("/p/a.rs", "FFFF"), None);
    assert_eq!(store.lookup("/p/other.rs", &tag), None);
}

#[test]
fn snapshot_store_caps_versions_per_path() {
    let store = SnapshotStore::new();
    let mut tags = Vec::new();
    for i in 0..5 {
        let text = format!("version {i}\n");
        tags.push(compute_tag(&text));
        store.record("/p/a.rs", &text);
    }
    // Only the last 4 versions survive.
    assert_eq!(store.lookup("/p/a.rs", &tags[0]), None, "oldest evicted");
    for tag in &tags[1..] {
        assert!(store.lookup("/p/a.rs", tag).is_some());
    }
}

#[test]
fn snapshot_store_dedups_same_tag() {
    let store = SnapshotStore::new();
    let a = "AAA\n";
    let ta = compute_tag(a);
    store.record("/p/x.rs", a);
    store.record("/p/x.rs", a); // dedup: still one version
    // Add three more distinct => 4 total, nothing evicted.
    for t in ["B\n", "C\n", "D\n"] {
        store.record("/p/x.rs", t);
    }
    assert!(store.lookup("/p/x.rs", &ta).is_some(), "deduped version not double-counted");
}

#[test]
fn snapshot_store_caps_paths_lru() {
    let store = SnapshotStore::new();
    for i in 0..=super::MAX_PATHS {
        // 31 distinct paths (0..=30)
        store.record(&format!("/p/{i}.rs"), &format!("body {i}\n"));
    }
    // The first-recorded (least-recently-used) path is evicted.
    assert_eq!(store.lookup("/p/0.rs", &compute_tag("body 0\n")), None);
    assert!(
        store
            .lookup(&format!("/p/{}.rs", super::MAX_PATHS), &compute_tag(&format!("body {}\n", super::MAX_PATHS)))
            .is_some()
    );
}
