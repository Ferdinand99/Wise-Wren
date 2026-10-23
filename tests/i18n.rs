//! Every language translates every one of Wren's own strings, with the same
//! placeholders and the plural forms its CLDR rule needs; every key the
//! code and templates use exists.

use std::collections::BTreeSet;

use vole_kit::i18n::{Lang, catalog};

/// Every language Wren ships is complete over the joined catalog (the
/// kit's strings and Wren's, the texts the kit takes from Wren included).
#[test]
fn every_language_is_complete() {
    wise_wren::install();
    catalog::assert_full_coverage();
}

/// The language picker's languages are exactly the ones Wren ships.
#[test]
fn wren_ships_every_language_the_kit_knows() {
    let shipped: BTreeSet<&str> = wise_wren::product::KIT
        .locales
        .iter()
        .map(|(c, _)| *c)
        .collect();
    let kit: BTreeSet<&str> = Lang::all().map(|l| l.code()).collect();
    assert_eq!(shipped, kit);
}

/// No language just copies English: each one translates most of Wren's
/// own strings (a few, like "Docker" or "Unraid", stay as they are).
#[test]
fn every_language_is_translated() {
    let catalogs: Vec<(&str, catalog::Catalog)> = wise_wren::product::KIT
        .locales
        .iter()
        .map(|(code, text)| (*code, catalog::parse(text).unwrap()))
        .collect();
    let en = &catalogs.iter().find(|(c, _)| *c == "en").unwrap().1;
    for (code, cat) in &catalogs {
        if *code == "en" {
            continue;
        }
        let same = en
            .iter()
            .filter(|(k, v)| cat.get(*k).is_some_and(|t| t == *v))
            .count();
        assert!(
            same * 10 < en.len(),
            "{code}: {same} of {} strings are English",
            en.len()
        );
    }
}

/// Every key Wren's templates and code use exists in its English catalog
/// (or the kit's).
#[test]
fn every_used_key_exists() {
    wise_wren::install();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut sources = Vec::new();
    for dir in ["templates", "src"] {
        collect(&root.join(dir), &mut sources);
    }
    let mut missing = Vec::new();
    let mut checked = 0;
    for file in sources {
        if file.components().any(|c| c.as_os_str() == "kit") {
            continue;
        }
        let src = std::fs::read_to_string(&file).unwrap();
        for call in ["tr(\"", "tr1(\"", "i18n::t(\"", "i18n::tf(\"", "label: \""] {
            let mut rest = src.as_str();
            while let Some(i) = rest.find(call) {
                let before = rest[..i].chars().next_back();
                rest = &rest[i + call.len()..];
                if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                    continue;
                }
                let Some(end) = rest.find('"') else { break };
                let key = &rest[..end];
                if !key.contains('.') || key.contains(' ') {
                    continue;
                }
                checked += 1;
                if catalog::get(Lang::EN, key).is_none() {
                    missing.push(format!("{}: {key}", file.display()));
                }
            }
        }
    }
    assert!(checked > 50, "only {checked} keys found");
    assert!(missing.is_empty(), "{}", missing.join("\n"));
}

fn collect(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs" || e == "html") {
            out.push(path);
        }
    }
}
