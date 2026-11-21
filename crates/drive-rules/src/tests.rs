//! The evaluator and the safety rules that are decided before anything
//! touches a disk: a new rule is a dry run and off, moves on a computer
//! are blocked unless the device allows them, Trash is never offered for a
//! computer's files, copies take free names, a rule cannot climb out of
//! its destination, and the Drive tree's actions never delete.

use drive_files::projects::{Part, Project};
use drive_files::tree as ft;
use drive_spine::kinds::Kinds;
use drive_spine::tags::{self, Library, NewTag, Target};
use loro::LoroDoc;

use crate::eval::{MOVES_OFF, NO_TRASH};
use crate::journal::{self, Entry, Preview, Row, Status, Undo};
use crate::template::day_ms;
use crate::*;

const DAY: i64 = 86_400_000;

fn now() -> i64 {
    day_ms("2026-11-21").unwrap() + 12 * 3_600_000
}

struct Fixture {
    kinds: Kinds,
    tags: LoroDoc,
    devices: Vec<DeviceInfo>,
    locations: Vec<LocInfo>,
    projects: Vec<Project>,
}

impl Fixture {
    fn new() -> Fixture {
        Fixture {
            kinds: Kinds::from(vec![]),
            tags: LoroDoc::new(),
            devices: vec![
                DeviceInfo {
                    id: "bs".into(),
                    name: "Battleship".into(),
                    roles: vec!["client".into()],
                },
                DeviceInfo {
                    id: "nas".into(),
                    name: "Starcommand".into(),
                    roles: vec!["storage-server".into()],
                },
            ],
            locations: vec![
                LocInfo {
                    id: "dl".into(),
                    name: "Downloads".into(),
                    device: "bs".into(),
                    in_storage: false,
                },
                LocInfo {
                    id: "arch".into(),
                    name: "Archive".into(),
                    device: "nas".into(),
                    in_storage: false,
                },
            ],
            projects: vec![Project {
                id: "p1".into(),
                name: "Song A".into(),
                kind: "MusicRecording".into(),
                created: 0,
                parts: vec![Part {
                    id: "pa".into(),
                    label: "Audio".into(),
                    location: "dl".into(),
                    path: "Song A".into(),
                    available: false,
                }],
                docs: vec![],
                shared_with: vec![],
            }],
        }
    }

    fn world<'a>(&'a self, lib: &'a Library, runner: Runner) -> World<'a> {
        World {
            now: now(),
            kinds: &self.kinds,
            tags: lib,
            devices: &self.devices,
            locations: &self.locations,
            projects: &self.projects,
            runner,
        }
    }
}

fn file(path: &str, size: u64, modified: i64) -> Facts {
    Facts {
        item: Some(Item::Location {
            device: "bs".into(),
            location: "dl".into(),
            path: path.into(),
        }),
        size,
        modified,
        content: format!("h-{path}"),
        copies: 1,
        holders: vec!["bs".into()],
        ..Facts::default()
    }
}

fn device(moves: bool) -> Runner {
    Runner::Device {
        id: "bs".into(),
        moves_allowed: moves,
    }
}

fn rule(when: Vec<Condition>, actions: Vec<Action>) -> Rule {
    Rule {
        id: "r1".into(),
        name: "Test".into(),
        enabled: true,
        mode: Mode::DryRun,
        when: When {
            all: true,
            groups: vec![Group {
                all: true,
                conditions: when,
            }],
        },
        actions,
        ..Rule::default()
    }
}

fn text(op: TextOp, v: &str) -> Text {
    Text {
        op,
        value: v.into(),
    }
}

#[test]
fn conditions_match_names_extensions_kinds_sizes_and_dates() {
    let fx = Fixture::new();
    let lib = Library::load(&fx.tags);
    let w = fx.world(&lib, device(false));
    let wav = file("Takes/Kick 01.WAV", 5_000_000, now() - 2 * DAY);
    let pdf = file("Charts/set.pdf", 80_000, now() - 40 * DAY);

    let check = |c: Condition, f: &Facts| {
        let r = rule(vec![c], vec![]);
        Matcher::new(&r).unwrap().matches(f, &w)
    };
    assert!(check(
        Condition::is(Test::Name {
            text: text(TextOp::StartsWith, "kick")
        }),
        &wav
    ));
    assert!(!check(
        Condition::is(Test::Name {
            text: text(TextOp::Is, "kick")
        }),
        &wav
    ));
    assert!(check(
        Condition::is(Test::Name {
            text: text(TextOp::Matches, r"\d{2}\.WAV$")
        }),
        &wav
    ));
    assert!(check(
        Condition::is(Test::Path {
            text: text(TextOp::Contains, "takes/")
        }),
        &wav
    ));
    assert!(check(
        Condition::is(Test::Extension {
            any: vec![".wav".into(), "aif".into()]
        }),
        &wav
    ));
    assert!(!check(
        Condition::is(Test::Extension {
            any: vec!["wav".into()]
        }),
        &pdf
    ));
    assert!(check(
        Condition::is(Test::Kind {
            kind: "audio".into(),
            family: true
        }),
        &wav
    ));
    assert!(check(
        Condition::not(Test::Kind {
            kind: "audio".into(),
            family: true
        }),
        &pdf
    ));
    assert!(check(
        Condition::is(Test::Size {
            cmp: Cmp::More,
            bytes: 1_000_000
        }),
        &wav
    ));
    assert!(check(
        Condition::is(Test::Size {
            cmp: Cmp::Less,
            bytes: 100_000
        }),
        &pdf
    ));
    let last_week = Range::InLast {
        n: 7,
        unit: Unit::Days,
    };
    assert!(check(
        Condition::is(Test::Date {
            of: DateOf::Modified,
            range: last_week.clone()
        }),
        &wav
    ));
    assert!(!check(
        Condition::is(Test::Date {
            of: DateOf::Modified,
            range: last_week.clone()
        }),
        &pdf
    ));
    assert!(check(
        Condition::is(Test::Date {
            of: DateOf::Modified,
            range: Range::OlderThan {
                n: 1,
                unit: Unit::Months
            }
        }),
        &pdf
    ));
    assert!(check(
        Condition::is(Test::Date {
            of: DateOf::Modified,
            range: Range::Before {
                day: "2026-11-01".into()
            }
        }),
        &pdf
    ));
    // A date the file system does not record never matches, negated or not.
    assert!(!check(
        Condition::is(Test::Date {
            of: DateOf::Created,
            range: last_week.clone()
        }),
        &wav
    ));
    assert!(!check(
        Condition::not(Test::Date {
            of: DateOf::Created,
            range: last_week
        }),
        &wav
    ));
    assert!(check(
        Condition::is(Test::Copies {
            cmp: Cmp::Less,
            n: 2
        }),
        &wav
    ));
    assert!(check(
        Condition::is(Test::HeldBy {
            device: "bs".into()
        }),
        &wav
    ));
    assert!(!check(
        Condition::is(Test::HeldBy {
            device: "nas".into()
        }),
        &wav
    ));
}

#[test]
fn groups_combine_all_and_any() {
    let fx = Fixture::new();
    let lib = Library::load(&fx.tags);
    let w = fx.world(&lib, device(false));
    let wav = file("a.wav", 10, now());
    let yes = Condition::is(Test::Extension {
        any: vec!["wav".into()],
    });
    let no = Condition::is(Test::Extension {
        any: vec!["mp3".into()],
    });
    let mut r = rule(vec![yes.clone(), no.clone()], vec![]);
    assert!(!Matcher::new(&r).unwrap().matches(&wav, &w));
    r.when.groups[0].all = false;
    assert!(Matcher::new(&r).unwrap().matches(&wav, &w));
    r.when.groups.push(Group {
        all: true,
        conditions: vec![no],
    });
    assert!(!Matcher::new(&r).unwrap().matches(&wav, &w));
    r.when.all = false;
    assert!(Matcher::new(&r).unwrap().matches(&wav, &w));
}

#[test]
fn tags_fields_and_projects() {
    let fx = Fixture::new();
    let drums = tags::create(
        &fx.tags,
        NewTag {
            name: "Drums",
            ..NewTag::default()
        },
        "me",
        1,
    )
    .unwrap();
    let kick = tags::create(
        &fx.tags,
        NewTag {
            name: "Kick",
            parents: &[drums.clone()],
            ..NewTag::default()
        },
        "me",
        1,
    )
    .unwrap();
    let f = file("Song A/kick.wav", 10, now());
    tags::tag(
        &fx.tags,
        &Target::Content(f.content.clone()),
        &kick,
        "me",
        1,
    )
    .unwrap();
    tags::add_field(
        &fx.tags,
        &Target::Content(f.content.clone()),
        "Artist",
        tags::FieldKind::Text,
        "Jane Doe",
        "me",
        1,
    )
    .unwrap();
    let lib = Library::load(&fx.tags);
    let w = fx.world(&lib, device(false));
    let m = |c: Condition, f: &Facts| Matcher::new(&rule(vec![c], vec![])).unwrap().matches(f, &w);
    // A parent tag finds its children.
    assert!(m(Condition::is(Test::Tag { tag: drums.clone() }), &f));
    assert!(!m(
        Condition::is(Test::Tag { tag: drums.clone() }),
        &file("other.wav", 1, 0)
    ));
    assert!(m(
        Condition::is(Test::Field {
            name: "artist".into(),
            text: text(TextOp::Contains, "jane")
        }),
        &f
    ));
    assert!(m(
        Condition::is(Test::Project {
            project: "p1".into()
        }),
        &f
    ));
    assert!(m(
        Condition::is(Test::Project {
            project: String::new()
        }),
        &f
    ));
    assert!(!m(
        Condition::is(Test::Project {
            project: String::new()
        }),
        &file("x.wav", 1, 0)
    ));

    // Scope by tag and by project.
    let mut r = rule(vec![], vec![]);
    r.scope.places = vec![Place::Tag { tag: drums }];
    assert!(Matcher::new(&r).unwrap().matches(&f, &w));
    r.scope.places = vec![Place::Location {
        location: "dl".into(),
        path: "Charts".into(),
    }];
    assert!(!Matcher::new(&r).unwrap().matches(&f, &w));
    assert!(
        Matcher::new(&r)
            .unwrap()
            .matches(&file("Charts/a.pdf", 1, 0), &w)
    );
    assert!(
        !Matcher::new(&r)
            .unwrap()
            .matches(&file("Charts2/a.pdf", 1, 0), &w)
    );
}

#[test]
fn bad_regexes_and_days_are_refused_up_front() {
    let r = rule(
        vec![Condition::is(Test::Name {
            text: text(TextOp::Matches, "("),
        })],
        vec![],
    );
    assert!(matches!(
        Matcher::new(&r),
        Err(eval::RuleError::Regex { .. })
    ));
    let r = rule(
        vec![Condition::is(Test::Date {
            of: DateOf::Modified,
            range: Range::After { day: "soon".into() },
        })],
        vec![],
    );
    assert!(matches!(Matcher::new(&r), Err(eval::RuleError::Day(_))));
}

#[test]
fn device_scope_by_role_and_name() {
    let fx = Fixture::new();
    let mut r = rule(vec![], vec![]);
    assert!(eval::runs_on(&r, &fx.devices[0]));
    r.scope.devices = Devices::Roles(vec!["storage-server".into()]);
    assert!(!eval::runs_on(&r, &fx.devices[0]));
    assert!(eval::runs_on(&r, &fx.devices[1]));
    r.scope.devices = Devices::Roles(vec!["workstation".into()]);
    assert!(eval::runs_on(&r, &fx.devices[0]));
    r.scope.devices = Devices::Named(vec!["nas".into()]);
    assert!(!eval::runs_on(&r, &fx.devices[0]));
}

#[test]
fn plans_fill_templates_and_say_what_they_would_do() {
    let fx = Fixture::new();
    let lib = Library::load(&fx.tags);
    let w = fx.world(&lib, device(true));
    let mut f = file("Song A/take.wav", 10, day_ms("2026-03-07").unwrap());
    f.created = Some(day_ms("2025-12-31").unwrap());
    let r = rule(
        vec![],
        vec![
            Action::Copy {
                to: Dest::Location {
                    location: "arch".into(),
                    path: "Sorted/{year}/{month}".into(),
                },
                name: "{project} - {name} ({device}).{ext}".into(),
            },
            Action::Rename {
                name: "{kind} {name}".into(),
            },
            Action::SetField {
                name: "Source".into(),
                value: "{device}".into(),
            },
        ],
    );
    let p = Matcher::new(&r).unwrap().plan(&f, &w);
    assert_eq!(p.steps.len(), 3);
    match &p.steps[0].effect {
        Effect::Copy {
            to:
                Spot::Location {
                    device,
                    location,
                    folder,
                    name,
                },
        } => {
            assert_eq!((device.as_str(), location.as_str()), ("nas", "arch"));
            assert_eq!(folder, "Sorted/2025/12");
            assert_eq!(name, "Song A - take (Battleship).wav");
        }
        other => panic!("{other:?}"),
    }
    assert!(
        p.steps[0].says.contains("Archive on Starcommand"),
        "{}",
        p.steps[0].says
    );
    match &p.steps[1].effect {
        Effect::Move { to } => assert_eq!(to.path(), "Song A/Audio take"),
        other => panic!("{other:?}"),
    }
    assert!(p.steps[1].blocked.is_none());
    assert_eq!(
        p.steps[2].effect,
        Effect::Field {
            name: "Source".into(),
            value: "Battleship".into()
        }
    );
}

#[test]
fn a_later_step_acts_where_a_move_put_the_file() {
    let fx = Fixture::new();
    let lib = Library::load(&fx.tags);
    let w = fx.world(&lib, device(true));
    let f = file("in/a.wav", 10, now());
    let r = rule(
        vec![],
        vec![
            Action::Move {
                to: Dest::Location {
                    location: "dl".into(),
                    path: "sorted".into(),
                },
                name: String::new(),
            },
            Action::Rename {
                name: "b.{ext}".into(),
            },
        ],
    );
    let p = Matcher::new(&r).unwrap().plan(&f, &w);
    match &p.steps[1].effect {
        Effect::Move { to } => assert_eq!(to.path(), "sorted/b.wav"),
        other => panic!("{other:?}"),
    }
}

// ── safety ──────────────────────────────────────────────────────────────

#[test]
fn a_new_rule_is_off_and_a_dry_run() {
    let d = LoroDoc::new();
    let mut r = rule(vec![], vec![Action::Rename { name: "x".into() }]);
    r.id.clear();
    r.enabled = true;
    r.mode = Mode::Live;
    let id = doc::save(&d, &r, "me", 5).unwrap();
    let back = doc::rule(&d, &id).unwrap();
    assert!(!back.enabled, "saved rules start off");
    assert_eq!(back.mode, Mode::DryRun, "saved rules start as dry runs");
    // Saving an edit keeps what a person switched.
    doc::set_enabled(&d, &id, true).unwrap();
    doc::set_mode(&d, &id, Mode::Live).unwrap();
    let mut edited = back.clone();
    edited.name = "Renamed".into();
    doc::save(&d, &edited, "me", 6).unwrap();
    let back = doc::find(&d, "renamed").unwrap();
    assert!(back.enabled && back.mode == Mode::Live);
    assert_eq!(back.actions, r.actions);
}

#[test]
fn moves_on_a_computer_are_blocked_unless_the_device_allows_them() {
    let fx = Fixture::new();
    let lib = Library::load(&fx.tags);
    let f = file("in/a.wav", 10, now());
    let r = rule(
        vec![],
        vec![
            Action::Rename {
                name: "b.wav".into(),
            },
            Action::Move {
                to: Dest::Location {
                    location: "dl".into(),
                    path: "out".into(),
                },
                name: String::new(),
            },
        ],
    );
    for runner in [
        device(false),
        Runner::Preview {
            moves_allowed: false,
        },
    ] {
        let p = Matcher::new(&r).unwrap().plan(&f, &fx.world(&lib, runner));
        assert!(
            p.steps
                .iter()
                .all(|s| s.blocked.as_deref() == Some(MOVES_OFF)),
            "{p:?}"
        );
        assert!(!p.does_something());
        // A blocked move does not change where later steps act.
        match &p.steps[1].effect {
            Effect::Move { to } => assert_eq!(to.path(), "out/a.wav"),
            other => panic!("{other:?}"),
        }
    }
    let p = Matcher::new(&r)
        .unwrap()
        .plan(&f, &fx.world(&lib, device(true)));
    assert!(p.steps.iter().all(|s| s.blocked.is_none()));
    // Even allowed, a move to another computer is not done (copy instead).
    let r = rule(
        vec![],
        vec![Action::Move {
            to: Dest::Location {
                location: "arch".into(),
                path: String::new(),
            },
            name: String::new(),
        }],
    );
    let p = Matcher::new(&r)
        .unwrap()
        .plan(&f, &fx.world(&lib, device(true)));
    assert!(p.steps[0].blocked.is_some());
}

#[test]
fn trash_is_never_offered_for_a_computers_files() {
    let fx = Fixture::new();
    let lib = Library::load(&fx.tags);
    let r = rule(vec![], vec![Action::Trash]);
    for runner in [
        device(true),
        Runner::Preview {
            moves_allowed: true,
        },
    ] {
        let p = Matcher::new(&r)
            .unwrap()
            .plan(&file("a.wav", 1, 0), &fx.world(&lib, runner));
        assert_eq!(p.steps[0].blocked.as_deref(), Some(NO_TRASH));
    }
    let drive = Facts {
        item: Some(Item::Drive {
            id: "1@2".into(),
            path: "a.wav".into(),
        }),
        ..Facts::default()
    };
    let p = Matcher::new(&r)
        .unwrap()
        .plan(&drive, &fx.world(&lib, Runner::Node));
    assert!(p.steps[0].blocked.is_none());
}

#[test]
fn templates_cannot_climb_out_of_their_destination() {
    let fx = Fixture::new();
    let lib = Library::load(&fx.tags);
    let w = fx.world(&lib, device(true));
    let r = rule(
        vec![],
        vec![Action::Copy {
            to: Dest::Location {
                location: "dl".into(),
                path: "../../etc/{name}".into(),
            },
            name: "../{name}".into(),
        }],
    );
    let p = Matcher::new(&r).unwrap().plan(&file("a.wav", 1, 0), &w);
    match &p.steps[0].effect {
        Effect::Copy { to } => {
            assert_eq!(to.folder(), "etc/a");
            assert_eq!(to.name(), "..-a");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_rule_copying_into_its_own_places_is_flagged() {
    let mut r = rule(
        vec![],
        vec![Action::Copy {
            to: Dest::Location {
                location: "dl".into(),
                path: "in/copies".into(),
            },
            name: String::new(),
        }],
    );
    r.scope.places = vec![Place::Location {
        location: "dl".into(),
        path: "in".into(),
    }];
    assert!(eval::feeds_itself(&r));
    r.scope.places = vec![Place::Location {
        location: "dl".into(),
        path: "other".into(),
    }];
    assert!(!eval::feeds_itself(&r));
}

#[test]
fn drive_tree_actions_never_delete_and_copies_take_free_names() {
    let files = LoroDoc::new();
    let songs = ft::mkdir(&files, None, "Songs").unwrap();
    let a = ft::create_file(&files, Some(songs), "mix.wav").unwrap();
    ft::set_content(&files, a, 3, "abc").unwrap();
    let taken = ft::create_file(&files, Some(songs), "mix (copy).wav").unwrap();
    ft::set_content(&files, taken, 9, "zzz").unwrap();
    let item = Item::Drive {
        id: ft::id_string(a),
        path: "Songs/mix.wav".into(),
    };

    // A copy beside a taken name gets " (2)"; the taken one is untouched.
    let (copy, undo_copy) = tree::apply_files(
        &files,
        &item,
        &Effect::Copy {
            to: Spot::Drive {
                folder: "Songs".into(),
                name: "mix (copy).wav".into(),
            },
        },
    )
    .unwrap();
    assert_eq!(copy.path(), "Songs/mix (copy) (2).wav");
    assert_eq!(ft::entry(&files, taken).unwrap().hash, "zzz");
    let Item::Drive { id: cid, .. } = copy.clone() else {
        panic!()
    };
    assert_eq!(
        ft::entry(&files, ft::parse_id(&cid).unwrap()).unwrap().hash,
        "abc"
    );

    // Trash is a folder; undo puts it back; nothing is ever gone.
    let (_, undo_trash) = tree::apply_files(&files, &item, &Effect::Trash).unwrap();
    assert!(ft::in_trash(&files, a));
    tree::undo_files(&files, &undo_trash).unwrap();
    assert_eq!(ft::path_of(&files, a), "Songs/mix.wav");

    // A move into a folder with that name taken lands beside it.
    let other = ft::mkdir(&files, None, "Done").unwrap();
    let there = ft::create_file(&files, Some(other), "mix.wav").unwrap();
    let (moved, undo_move) = tree::apply_files(
        &files,
        &item,
        &Effect::Move {
            to: Spot::Drive {
                folder: "Done".into(),
                name: "mix.wav".into(),
            },
        },
    )
    .unwrap();
    assert_eq!(moved.path(), "Done/mix (2).wav");
    assert_eq!(ft::path_of(&files, there), "Done/mix.wav");
    tree::undo_files(&files, &undo_move).unwrap();
    assert_eq!(ft::path_of(&files, a), "Songs/mix.wav");

    // Undoing a copy trashes the copy (it is not deleted).
    tree::undo_files(&files, &undo_copy).unwrap();
    assert!(ft::in_trash(&files, ft::parse_id(&cid).unwrap()));
    assert_eq!(ft::files(&files).len(), 4, "every entry is still there");
}

#[test]
fn tag_and_field_actions_undo() {
    let doc = LoroDoc::new();
    let t = tags::create(
        &doc,
        NewTag {
            name: "Mix",
            ..NewTag::default()
        },
        "me",
        1,
    )
    .unwrap();
    let target = Target::Content("h".into());
    let u = tree::apply_tags(
        &doc,
        &target,
        &Effect::Tag {
            tag: t.clone(),
            on: true,
        },
        "rule",
        2,
    )
    .unwrap();
    assert_eq!(Library::load(&doc).on(&target).tag_ids(), vec![t.clone()]);
    tree::undo_tags(&doc, &u, "me", 3).unwrap();
    assert!(Library::load(&doc).on(&target).tags.is_empty());

    let u1 = tree::apply_tags(
        &doc,
        &target,
        &Effect::Field {
            name: "Source".into(),
            value: "A".into(),
        },
        "rule",
        2,
    )
    .unwrap();
    let u2 = tree::apply_tags(
        &doc,
        &target,
        &Effect::Field {
            name: "source".into(),
            value: "B".into(),
        },
        "rule",
        2,
    )
    .unwrap();
    assert_eq!(Library::load(&doc).on(&target).fields[0].value, "B");
    tree::undo_tags(&doc, &u2, "me", 3).unwrap();
    assert_eq!(Library::load(&doc).on(&target).fields[0].value, "A");
    tree::undo_tags(&doc, &u1, "me", 3).unwrap();
    assert!(Library::load(&doc).on(&target).fields.is_empty());
}

#[test]
fn noops_are_not_redone() {
    let fx = Fixture::new();
    let t = tags::create(
        &fx.tags,
        NewTag {
            name: "Mix",
            ..NewTag::default()
        },
        "me",
        1,
    )
    .unwrap();
    let f = file("Song A/a.wav", 1, 0);
    tags::tag(&fx.tags, &Target::Content(f.content.clone()), &t, "me", 1).unwrap();
    let lib = Library::load(&fx.tags);
    let w = fx.world(&lib, device(true));
    let r = rule(
        vec![],
        vec![
            Action::AddTags {
                tags: vec![t.clone(), "missing".into()],
            },
            Action::AddToProject {
                project: "p1".into(),
            },
            Action::Rename {
                name: "{name}.{ext}".into(),
            },
        ],
    );
    let p = Matcher::new(&r).unwrap().plan(&f, &w);
    assert_eq!(p.steps.len(), 4, "one step per tag");
    assert!(p.steps[0].noop);
    assert!(p.steps[1].blocked.is_some());
    assert!(p.steps[2].noop, "already in the project");
    assert!(p.steps[3].noop, "same name");
    assert!(!p.does_something());
}

#[test]
fn the_journal_and_previews_round_trip() {
    let d = LoroDoc::new();
    let e = Entry {
        run: "run1".into(),
        device: "bs".into(),
        rule: "r1".into(),
        rule_name: "Test".into(),
        at: 10,
        action: "copy".into(),
        says: "copy to …".into(),
        before: Some(Item::Location {
            device: "bs".into(),
            location: "dl".into(),
            path: "a.wav".into(),
        }),
        status: Some(Status::Waiting),
        undo: Undo::DeviceCopy {
            location: "arch".into(),
            path: "a.wav".into(),
            full: "f".into(),
        },
        for_device: "nas".into(),
        ..Entry::default()
    };
    let id = journal::record(&d, &e).unwrap();
    let back = journal::entry(&d, &id).unwrap();
    assert_eq!(back.undo, e.undo);
    assert_eq!(back.status, Some(Status::Waiting));
    assert!(journal::request_undo(&d, &id, 20).unwrap());
    assert_eq!(journal::entry(&d, &id).unwrap().undo_requested, 20);

    let fx = Fixture::new();
    let lib = Library::load(&fx.tags);
    let w = fx.world(&lib, device(false));
    let r = rule(vec![], vec![Action::Trash]);
    let plan = Matcher::new(&r).unwrap().plan(&file("a.wav", 1, 0), &w);
    journal::put_preview(
        &d,
        &Preview {
            rule: "r1".into(),
            device: "bs".into(),
            at: 5,
            matched: 1,
            rows: vec![Row::of(&plan)],
        },
    )
    .unwrap();
    let p = journal::previews(&d, "r1");
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].rows[0].steps[0].blocked.as_deref(), Some(NO_TRASH));
}

#[test]
fn bundles_stay_on_their_computer_for_now() {
    let fx = Fixture::new();
    let lib = Library::load(&fx.tags);
    let w = fx.world(&lib, device(true));
    let mut f = file("Song.logicx", 10, 0);
    f.bundle = true;
    f.content.clear();
    let r = rule(
        vec![],
        vec![
            Action::Copy {
                to: Dest::Location {
                    location: "dl".into(),
                    path: "Backup".into(),
                },
                name: String::new(),
            },
            Action::Copy {
                to: Dest::Location {
                    location: "arch".into(),
                    path: String::new(),
                },
                name: String::new(),
            },
            Action::Copy {
                to: Dest::Drive {
                    path: String::new(),
                },
                name: String::new(),
            },
            Action::HighlyAvailable,
            Action::AddTags {
                tags: vec!["x".into()],
            },
        ],
    );
    let p = Matcher::new(&r).unwrap().plan(&f, &w);
    assert!(
        p.steps[0].blocked.is_none(),
        "a copy beside it on the same computer is fine"
    );
    assert!(p.steps[1..].iter().all(|s| s.blocked.is_some()), "{p:?}");
}
