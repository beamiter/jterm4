mod common;

use jterm4::state::{PaneLayout, escape_tab_state, unescape_tab_state, parse_tabs_state};

#[test]
fn test_escape_unescape_roundtrip() {
    let inputs = vec![
        "hello world",
        "line\nbreak",
        "tab\there",
        "back\\slash",
        "mixed\t\n\\all",
    ];

    for input in inputs {
        let escaped = escape_tab_state(input);
        let unescaped = unescape_tab_state(&escaped);
        assert_eq!(input, unescaped, "Roundtrip failed for: {}", input);
    }
}

#[test]
fn test_pane_layout_leaf_serialization() {
    let layout = PaneLayout::Leaf {
        dir: "/tmp".to_string(),
        sid: "123-456".to_string(),
        cmds: None,
    };

    let json = serde_json::to_string(&layout).expect("Serialization failed");
    let deserialized: PaneLayout = serde_json::from_str(&json).expect("Deserialization failed");

    match deserialized {
        PaneLayout::Leaf { dir, sid, cmds } => {
            assert_eq!(dir, "/tmp");
            assert_eq!(sid, "123-456");
            assert_eq!(cmds, None);
        }
        _ => panic!("Expected Leaf layout"),
    }
}

#[test]
fn test_pane_layout_leaf_with_commands() {
    let layout = PaneLayout::Leaf {
        dir: "/home".to_string(),
        sid: "789-012".to_string(),
        cmds: Some("nix develop".to_string()),
    };

    let json = serde_json::to_string(&layout).expect("Serialization failed");
    let deserialized: PaneLayout = serde_json::from_str(&json).expect("Deserialization failed");

    match deserialized {
        PaneLayout::Leaf { dir, sid, cmds } => {
            assert_eq!(dir, "/home");
            assert_eq!(sid, "789-012");
            assert_eq!(cmds, Some("nix develop".to_string()));
        }
        _ => panic!("Expected Leaf layout"),
    }
}

#[test]
fn test_pane_layout_split_serialization() {
    let layout = PaneLayout::Split {
        orientation: 'h',
        position: 500,
        start: Box::new(PaneLayout::Leaf {
            dir: "/tmp".to_string(),
            sid: "123-456".to_string(),
            cmds: None,
        }),
        end: Box::new(PaneLayout::Leaf {
            dir: "/home".to_string(),
            sid: "789-012".to_string(),
            cmds: Some("nix develop".to_string()),
        }),
    };

    let json = serde_json::to_string(&layout).expect("Serialization failed");
    let deserialized: PaneLayout = serde_json::from_str(&json).expect("Deserialization failed");

    match deserialized {
        PaneLayout::Split {
            orientation,
            position,
            start,
            end,
        } => {
            assert_eq!(orientation, 'h');
            assert_eq!(position, 500);

            match *start {
                PaneLayout::Leaf { ref dir, ref sid, .. } => {
                    assert_eq!(dir, "/tmp");
                    assert_eq!(sid, "123-456");
                }
                _ => panic!("Expected Leaf in start"),
            }

            match *end {
                PaneLayout::Leaf { ref dir, ref sid, ref cmds } => {
                    assert_eq!(dir, "/home");
                    assert_eq!(sid, "789-012");
                    assert_eq!(cmds, &Some("nix develop".to_string()));
                }
                _ => panic!("Expected Leaf in end"),
            }
        }
        _ => panic!("Expected Split layout"),
    }
}

#[test]
fn test_pane_layout_nested_splits() {
    let layout = PaneLayout::Split {
        orientation: 'h',
        position: 500,
        start: Box::new(PaneLayout::Leaf {
            dir: "/tmp".to_string(),
            sid: "123-456".to_string(),
            cmds: None,
        }),
        end: Box::new(PaneLayout::Split {
            orientation: 'v',
            position: 300,
            start: Box::new(PaneLayout::Leaf {
                dir: "/home".to_string(),
                sid: "789-012".to_string(),
                cmds: None,
            }),
            end: Box::new(PaneLayout::Leaf {
                dir: "/var".to_string(),
                sid: "345-678".to_string(),
                cmds: None,
            }),
        }),
    };

    let json = serde_json::to_string(&layout).expect("Serialization failed");
    let deserialized: PaneLayout = serde_json::from_str(&json).expect("Deserialization failed");

    // Verify structure is preserved
    match deserialized {
        PaneLayout::Split { orientation, .. } => {
            assert_eq!(orientation, 'h');
        }
        _ => panic!("Expected outer Split"),
    }
}

#[test]
fn test_parse_tabs_state_legacy_format() {
    let contents = r#"current_page=0
tab=Terminal 1	/tmp	123-456	nix develop
tab=Terminal 2	/home	789-012"#;

    let (current, tabs) = parse_tabs_state(contents);

    assert_eq!(current, Some(0));
    assert_eq!(tabs.len(), 2);

    // First tab
    match &tabs[0].1 {
        PaneLayout::Leaf { dir, sid, cmds } => {
            assert_eq!(dir, "/tmp");
            assert_eq!(sid, "123-456");
            assert_eq!(cmds, &Some("nix develop".to_string()));
        }
        _ => panic!("Expected Leaf"),
    }

    // Second tab
    match &tabs[1].1 {
        PaneLayout::Leaf { dir, sid, cmds } => {
            assert_eq!(dir, "/home");
            assert_eq!(sid, "789-012");
            assert_eq!(cmds, &None);
        }
        _ => panic!("Expected Leaf"),
    }
}

#[test]
fn test_parse_tabs_state_new_json_format() {
    let leaf_json = serde_json::json!({
        "type": "leaf",
        "dir": "/tmp",
        "sid": "123-456",
        "cmds": "nix develop"
    });

    let contents = format!(
        r#"current_page=0
tab=Terminal 1	{}"#,
        leaf_json.to_string()
    );

    let (current, tabs) = parse_tabs_state(&contents);

    assert_eq!(current, Some(0));
    assert_eq!(tabs.len(), 1);

    match &tabs[0].1 {
        PaneLayout::Leaf { dir, sid, cmds } => {
            assert_eq!(dir, "/tmp");
            assert_eq!(sid, "123-456");
            assert_eq!(cmds, &Some("nix develop".to_string()));
        }
        _ => panic!("Expected Leaf"),
    }
}

#[test]
fn test_parse_tabs_state_empty() {
    let contents = "";
    let (current, tabs) = parse_tabs_state(contents);

    assert_eq!(current, None);
    assert_eq!(tabs.len(), 0);
}

#[test]
fn test_parse_tabs_state_only_current_page() {
    let contents = "current_page=2";
    let (current, tabs) = parse_tabs_state(contents);

    assert_eq!(current, Some(2));
    assert_eq!(tabs.len(), 0);
}
