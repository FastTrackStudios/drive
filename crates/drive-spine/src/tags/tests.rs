use super::*;
use loro::ExportMode;

fn new(d: &LoroDoc, name: &str, parents: &[String]) -> String {
    create(
        d,
        NewTag {
            name,
            parents,
            ..NewTag::default()
        },
        "cody@example.com",
        1_000,
    )
    .unwrap()
}

fn sync(a: &LoroDoc, b: &LoroDoc) {
    a.commit();
    b.commit();
    a.import(&b.export(ExportMode::Snapshot).unwrap()).unwrap();
    b.import(&a.export(ExportMode::Snapshot).unwrap()).unwrap();
}

#[test]
fn tags_with_names_colours_aliases_and_parents() {
    let d = LoroDoc::new();
    let music = new(&d, "Music", &[]);
    let drums = new(&d, "Drums", std::slice::from_ref(&music));
    set_shorthand(&d, &drums, "dr").unwrap();
    set_color(&d, &drums, Color::Red).unwrap();
    add_alias(&d, &drums, "Kit").unwrap();
    add_alias(&d, &drums, "kit").unwrap(); // once, whatever its case
    add_alias(&d, &drums, "Percussion").unwrap();
    set_category(&d, &music, true).unwrap();
    d.commit();

    let lib = Library::load(&d);
    let t = lib.get(&drums).unwrap();
    assert_eq!(t.name, "Drums");
    assert_eq!(t.shorthand, "dr");
    assert_eq!(t.color, Color::Red);
    assert_eq!(t.aliases, ["Percussion", "kit"]);
    assert_eq!(t.parents, [music.clone()]);
    assert_eq!(t.by, "cody@example.com");
    assert!(lib.get(&music).unwrap().category);
    assert_eq!(lib.resolve("KIT"), [drums.clone()]);
    assert_eq!(lib.resolve("dr"), [drums.clone()]);
    assert_eq!(lib.descendants(&music).len(), 2);
    assert_eq!(lib.children(&music)[0].id, drums);
    // Built-ins are there without being written.
    assert!(lib.get(ARCHIVED).unwrap().hidden);
    assert!(lib.get(FAVORITE).unwrap().builtin);

    remove_alias(&d, &drums, "KIT").unwrap();
    rename(&d, &drums, "Drum kit").unwrap();
    assert_eq!(rename(&d, &drums, "  "), Err(TagError::NoName));
    let lib = Library::load(&d);
    assert_eq!(lib.get(&drums).unwrap().aliases, ["Percussion"]);
    assert_eq!(lib.get(&drums).unwrap().name, "Drum kit");
}

#[test]
fn a_parent_that_makes_a_cycle_is_refused() {
    let d = LoroDoc::new();
    let a = new(&d, "A", &[]);
    let b = new(&d, "B", std::slice::from_ref(&a));
    let c = new(&d, "C", std::slice::from_ref(&b));
    assert!(matches!(
        add_parent(&d, &a, &c),
        Err(TagError::Cycle { .. })
    ));
    assert!(matches!(
        add_parent(&d, &a, &a),
        Err(TagError::Cycle { .. })
    ));
    assert_eq!(
        add_parent(&d, &a, "nope"),
        Err(TagError::NotFound("nope".into()))
    );
    // Several parents are fine.
    let x = new(&d, "X", &[]);
    add_parent(&d, &c, &x).unwrap();
    let lib = Library::load(&d);
    assert_eq!(lib.ancestors(&c).len(), 4);
    assert!(lib.broken().is_empty());
}

#[test]
fn a_cycle_merged_from_two_devices_reads_the_same_everywhere() {
    let a = LoroDoc::new();
    let x = new(&a, "X", &[]);
    let y = new(&a, "Y", &[]);
    let b = LoroDoc::new();
    sync(&a, &b);
    // Each device alone is fine; together they make X ⊂ Y ⊂ X.
    add_parent(&a, &x, &y).unwrap();
    add_parent(&b, &y, &x).unwrap();
    sync(&a, &b);
    let (la, lb) = (Library::load(&a), Library::load(&b));
    assert_eq!(la, lb);
    assert_eq!(la.broken().len(), 1);
    // Walking terminates and one edge stands.
    assert_eq!(la.descendants(&x).len() + la.descendants(&y).len(), 3);
    // Once the edge is taken away for good, nothing is broken.
    let (c, p) = la.broken()[0].clone();
    remove_parent(&a, &c, &p).unwrap();
    a.commit();
    assert!(Library::load(&a).broken().is_empty());
}

#[test]
fn tagging_follows_the_target_and_records_who() {
    let d = LoroDoc::new();
    let mix = new(&d, "Mix", &[]);
    let file = Target::File("7@42".into());
    let content = Target::Content("abc123".into());
    tag(&d, &file, &mix, "cody@example.com", 5).unwrap();
    tag(&d, &content, &mix, "mia@example.com", 6).unwrap();
    tag(&d, &content, FAVORITE, "mia@example.com", 7).unwrap();
    assert_eq!(
        tag(&d, &content, "nope", "x", 8),
        Err(TagError::NotFound("nope".into()))
    );
    let f = add_field(&d, &content, "Artist", FieldKind::Text, "Jane", "mia", 9).unwrap();
    add_field(
        &d,
        &content,
        "Recorded",
        FieldKind::DateTime,
        "2026-11-21T19:30",
        "mia",
        10,
    )
    .unwrap();
    set_field(&d, &content, &f, "Jane Doe", "cody", 11).unwrap();
    d.commit();

    let lib = Library::load(&d);
    let on = lib.on(&content);
    assert_eq!(on.tag_ids(), [mix.clone(), FAVORITE.to_string()]);
    assert_eq!(on.tags[0].by, "mia@example.com");
    assert_eq!(on.fields[0].value, "Jane Doe");
    assert_eq!(on.fields[0].by, "cody");
    assert_eq!(on.fields[1].kind, FieldKind::DateTime);
    assert_eq!(lib.tagged(&mix).len(), 2);
    assert_eq!(lib.counts()[&mix], 2);

    untag(&d, &content, &mix).unwrap();
    remove_field(&d, &content, &f).unwrap();
    d.commit();
    let lib = Library::load(&d);
    assert_eq!(lib.on(&content).tag_ids(), [FAVORITE.to_string()]);
    assert_eq!(lib.on(&content).fields.len(), 1);
}

#[test]
fn deleting_a_tag_takes_it_off_everything() {
    let d = LoroDoc::new();
    let p = new(&d, "Parent", &[]);
    let c = new(&d, "Child", std::slice::from_ref(&p));
    let t = Target::Doc("studio:song:x".into());
    tag(&d, &t, &p, "cody", 1).unwrap();
    tag(&d, &t, &c, "cody", 2).unwrap();
    delete(&d, &p).unwrap();
    d.commit();
    let lib = Library::load(&d);
    assert!(lib.get(&p).is_none());
    assert!(lib.get(&c).unwrap().parents.is_empty());
    assert_eq!(lib.on(&t).tag_ids(), [c]);
    assert_eq!(
        delete(&d, ARCHIVED),
        Err(TagError::Builtin(ARCHIVED.into()))
    );
}

#[test]
fn people_tag_at_once_and_it_merges() {
    let a = LoroDoc::new();
    crate::history::prepare(&a, "cody@example.com");
    let drums = new(&a, "Drums", &[]);
    let b = LoroDoc::new();
    crate::history::prepare(&b, "mia@example.com");
    sync(&a, &b);

    let t = Target::Content("h1".into());
    // Both tag the same file; one renames the tag while the other colours it;
    // both make a tag called "Live" and both tag the file with theirs.
    tag(&a, &t, &drums, "cody@example.com", 10).unwrap();
    tag(&b, &t, FAVORITE, "mia@example.com", 11).unwrap();
    rename(&a, &drums, "Drum kit").unwrap();
    set_color(&b, &drums, Color::Teal).unwrap();
    let live_a = new(&a, "Live", &[]);
    let live_b = new(&b, "Live", &[]);
    tag(&a, &t, &live_a, "cody@example.com", 12).unwrap();
    tag(&b, &t, &live_b, "mia@example.com", 12).unwrap();
    crate::history::commit(&a, "cody@example.com");
    crate::history::commit(&b, "mia@example.com");
    sync(&a, &b);

    let (la, lb) = (Library::load(&a), Library::load(&b));
    assert_eq!(la, lb);
    let d = la.get(&drums).unwrap();
    assert_eq!((d.name.as_str(), d.color), ("Drum kit", Color::Teal));
    let on = la.on(&t);
    assert_eq!(on.tags.len(), 4);
    assert_ne!(live_a, live_b, "two tags, both kept");
    assert_eq!(la.resolve("live").len(), 2);
    // Who did what is in the history.
    let who: Vec<String> = crate::history::changes(&a)
        .into_iter()
        .map(|c| c.who)
        .collect();
    assert!(who.iter().any(|w| w == "cody@example.com"));
    assert!(who.iter().any(|w| w == "mia@example.com"));
}

#[test]
fn same_names_are_told_apart_by_their_parent() {
    let d = LoroDoc::new();
    let fnaf = new(&d, "Five Nights", &[]);
    set_shorthand(&d, &fnaf, "FNAF").unwrap();
    let other = new(&d, "Mercury", &[]);
    let f1 = new(&d, "Freddy", std::slice::from_ref(&fnaf));
    let f2 = new(&d, "Freddy", std::slice::from_ref(&other));
    let solo = new(&d, "Solo", std::slice::from_ref(&fnaf));
    d.commit();
    let lib = Library::load(&d);
    assert_eq!(lib.label(&f1), "Freddy (FNAF)");
    assert_eq!(lib.label(&f2), "Freddy (Mercury)");
    assert_eq!(lib.label(&solo), "Solo");
}

#[test]
fn suggestions_rank_exact_then_prefix_then_contains() {
    let d = LoroDoc::new();
    let snare = new(&d, "Snare", &[]);
    let sn = new(&d, "Sn", &[]);
    let acoustic = new(&d, "Acoustic snare", &[]);
    let unsnap = new(&d, "Unsnapped", &[]);
    d.commit();
    let lib = Library::load(&d);
    let ids: Vec<&str> = lib.suggest("sn").iter().map(|t| t.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            sn.as_str(),
            snare.as_str(),
            acoustic.as_str(),
            unsnap.as_str()
        ]
    );
    assert!(lib.suggest("  ").is_empty());
}

#[test]
fn the_inspector_groups_by_category() {
    let d = LoroDoc::new();
    let inst = new(&d, "Instruments", &[]);
    set_category(&d, &inst, true).unwrap();
    let genre = new(&d, "Genre", &[]);
    set_category(&d, &genre, true).unwrap();
    let drums = new(&d, "Drums", std::slice::from_ref(&inst));
    let rock = new(&d, "Rock", std::slice::from_ref(&genre));
    let loose = new(&d, "Loose", &[]);
    d.commit();
    let lib = Library::load(&d);
    let g = lib.grouped(&[rock.clone(), loose.clone(), drums.clone()]);
    assert_eq!(
        g,
        vec![
            (Some(genre), vec![rock]),
            (Some(inst), vec![drums]),
            (None, vec![loose]),
        ]
    );
}

#[test]
fn colours_and_kinds_parse_their_names() {
    for c in Color::ALL {
        assert_eq!(Color::parse(c.id()), c);
    }
    assert_eq!(Color::parse("Gray"), Color::Grey);
    assert_eq!(Color::parse("chartreuse"), Color::None);
    for k in FieldKind::ALL {
        assert_eq!(FieldKind::parse(k.id()), k);
    }
    assert!(
        templates()
            .iter()
            .any(|(n, k)| *n == "Notes" && *k == FieldKind::Multiline)
    );
}
