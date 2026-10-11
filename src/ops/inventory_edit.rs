//! Edits config.toml in place with `toml_edit`, so the user's comments, ordering
//! and formatting survive when Muxen adds or removes machines and runners.
//! Every function works on text, which lets demo mode reuse them on its
//! in-memory inventory.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;

use toml_edit::{value, Array, ArrayOfTables, DocumentMut, Item, Table};

use crate::config::{assign_machine_id, display_path, write_atomic, Inventory};

/// Serializes edits so two operations finishing together don't drop each other's change.
static FILE_LOCK: Mutex<()> = Mutex::new(());

/// Reads the file (missing is fine), applies `edit`, validates and writes it back atomically.
pub fn edit_file(path: &Path, edit: impl FnOnce(&str) -> Result<String, String>) -> Result<(), String> {
    let _guard = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("Couldn't read {}: {e}", display_path(path))),
    };
    let updated = edit(&text)?;
    write_atomic(path, updated.as_bytes()).map_err(|e| format!("Couldn't write {}: {e:#}", display_path(path)))
}

/// Appends a `[[machine]]` with `name` and `host`. Returns the new text and the machine's id.
pub fn add_machine(text: &str, name: &str, host: &str) -> Result<(String, String), String> {
    let name = name.trim();
    let host = host.trim();
    let mut doc = parse(text)?;
    // Comment lines right after the last table (before a blank line), like a
    // commented-out `# runners = [...]`, belong to that table: keep them above the
    // new one. The rest of the trailing comments stay at the end of the file.
    let trailing = doc.trailing().as_str().unwrap_or("").to_string();
    let (attached, rest) = split_attached(&trailing);
    let (attached, rest) = (attached.to_string(), rest.to_string());
    {
        let machines = machines_mut(&mut doc)?;
        if machines.iter().any(|t| table_name(t).eq_ignore_ascii_case(name)) {
            return Err(format!("A machine called \"{name}\" is already in config.toml."));
        }
        let mut table = Table::new();
        table["name"] = value(name);
        table["host"] = value(host);
        // A blank line before the header keeps the file readable.
        table.decor_mut().set_prefix(format!("{attached}\n"));
        machines.push(table);
    }
    doc.set_trailing(rest);
    let out = doc.to_string();
    let id = Inventory::parse(&out)
        .map_err(|e| format!("config.toml would be invalid: {e}"))?
        .machines
        .last()
        .map(|m| m.id.clone())
        .ok_or("The machine wasn't added.")?;
    Ok((out, id))
}

/// Removes the machine with this id (as `Inventory::parse` assigns ids).
pub fn remove_machine(text: &str, machine_id: &str) -> Result<String, String> {
    let mut doc = parse(text)?;
    let machines = machines_mut(&mut doc)?;
    let index = find_machine(machines, machine_id)?;
    let prefix = machines.get(index).and_then(|t| t.decor().prefix()).and_then(|p| p.as_str()).unwrap_or("").to_string();
    machines.remove(index);

    // Comments above the first [[machine]] belong to it in toml_edit, but the part
    // before the last blank line is the file's header. Keep that.
    let header = if index == 0 { file_header(&prefix) } else { "" };
    if !header.is_empty() {
        match machines.get_mut(0) {
            Some(next) => {
                let rest = next.decor().prefix().and_then(|p| p.as_str()).unwrap_or("").trim_start_matches('\n').to_string();
                next.decor_mut().set_prefix(format!("{header}{rest}"));
            }
            None => {
                let trailing = doc.trailing().as_str().unwrap_or("").to_string();
                doc.set_trailing(format!("{header}{trailing}"));
            }
        }
    }
    finish(doc)
}

/// Gives a machine a new display name. Its id stays the same: when the new
/// name wouldn't slug to it, the old id is written as `id`, so the machine's
/// runners, sessions, favorites and saved state stay attached to it.
pub fn rename_machine(text: &str, machine_id: &str, name: &str) -> Result<String, String> {
    let name = name.trim();
    let mut doc = parse(text)?;
    let machines = machines_mut(&mut doc)?;
    let index = find_machine(machines, machine_id)?;
    if machines.iter().enumerate().any(|(i, t)| i != index && table_name(t).eq_ignore_ascii_case(name)) {
        return Err(format!("A machine called \"{name}\" is already in config.toml."));
    }
    let table = machines.get_mut(index).ok_or("Machine not found.")?;
    // Keep a comment after the old name (`name = "Box" # the NAS`).
    match table.get_mut("name").and_then(Item::as_value_mut) {
        Some(v) => {
            let decor = v.decor().clone();
            *v = name.into();
            *v.decor_mut() = decor;
        }
        None => table["name"] = value(name),
    }
    let explicit = table.get("id").and_then(Item::as_str).is_some_and(|s| !s.trim().is_empty());
    if !explicit && crate::util::slug(name) != machine_id {
        table["id"] = value(machine_id);
    }
    let out = finish(doc)?;
    let kept = Inventory::parse(&out).map_err(|e| e.to_string())?.machines.iter().any(|m| m.id == machine_id && m.name == name);
    if !kept {
        return Err("Renaming it would change its id, so it wasn't renamed.".into());
    }
    Ok(out)
}

/// Records that a machine is a cloud machine (or forgets it, with `None`).
pub fn set_machine_cloud(text: &str, machine_id: &str, cloud: Option<&crate::config::CloudRef>) -> Result<String, String> {
    let mut doc = parse(text)?;
    let machines = machines_mut(&mut doc)?;
    let index = find_machine(machines, machine_id)?;
    let table = machines.get_mut(index).ok_or("That machine isn't in config.toml.")?;
    match cloud {
        Some(c) => {
            let mut t = toml_edit::InlineTable::new();
            t.insert("provider", c.provider.as_str().into());
            t.insert("id", (c.id as i64).into());
            t.insert("region", c.region.as_str().into());
            t.insert("size", c.size.as_str().into());
            t.insert("price_hourly", c.price_hourly.into());
            t.insert("created_at", c.created_at.as_str().into());
            table["cloud"] = value(t);
        }
        None => {
            table.remove("cloud");
        }
    }
    finish(doc)
}

/// Splits document trailing text into the comment lines glued to the last table and the rest.
fn split_attached(trailing: &str) -> (&str, &str) {
    if trailing.is_empty() || trailing.starts_with('\n') {
        return ("", trailing);
    }
    match trailing.find("\n\n") {
        Some(i) => (&trailing[..=i], &trailing[i + 1..]),
        None => (trailing, ""),
    }
}

/// The leading part of a decor prefix up to and including its last blank line.
fn file_header(prefix: &str) -> &str {
    match prefix.rfind("\n\n") {
        Some(i) => &prefix[..i + 2],
        None => "",
    }
}

/// Adds runner names to a machine's `runners` list (creating it if needed). Names already listed are skipped.
pub fn add_runners(text: &str, machine_id: &str, names: &[String]) -> Result<String, String> {
    let mut doc = parse(text)?;
    let machines = machines_mut(&mut doc)?;
    let index = find_machine(machines, machine_id)?;
    let table = machines.get_mut(index).ok_or("Machine not found.")?;
    match table.get_mut("runners") {
        Some(item) => {
            let list = item.as_array_mut().ok_or("`runners` in config.toml isn't a list.")?;
            for n in names {
                if !list.iter().any(|v| v.as_str().is_some_and(|s| s.eq_ignore_ascii_case(n))) {
                    list.push(n.as_str());
                }
            }
        }
        None => {
            let mut list = Array::new();
            for n in names {
                list.push(n.as_str());
            }
            table["runners"] = value(list);
        }
    }
    finish(doc)
}

/// Removes a runner name from a machine's `runners` list. Not listed is not an error.
pub fn remove_runner(text: &str, machine_id: &str, name: &str) -> Result<String, String> {
    let mut doc = parse(text)?;
    let machines = machines_mut(&mut doc)?;
    let index = find_machine(machines, machine_id)?;
    let table = machines.get_mut(index).ok_or("Machine not found.")?;
    if let Some(list) = table.get_mut("runners").and_then(Item::as_array_mut) {
        list.retain(|v| !v.as_str().is_some_and(|s| s.eq_ignore_ascii_case(name)));
    }
    finish(doc)
}

/// Allows or forbids agent workspaces on a machine. Allowed is the default, so
/// allowing removes the key rather than writing `workspaces = true`.
pub fn set_workspaces(text: &str, machine_id: &str, allowed: bool) -> Result<String, String> {
    let mut doc = parse(text)?;
    let machines = machines_mut(&mut doc)?;
    let index = find_machine(machines, machine_id)?;
    let table = machines.get_mut(index).ok_or("Machine not found.")?;
    if allowed {
        table.remove("workspaces");
    } else {
        table["workspaces"] = value(false);
    }
    finish(doc)
}

fn parse(text: &str) -> Result<DocumentMut, String> {
    text.parse::<DocumentMut>().map_err(|e| format!("config.toml has a syntax error, so it wasn't changed: {}", e.to_string().trim()))
}

fn finish(doc: DocumentMut) -> Result<String, String> {
    let out = doc.to_string();
    Inventory::parse(&out).map_err(|e| format!("config.toml would be invalid: {e}"))?;
    Ok(out)
}

fn machines_mut(doc: &mut DocumentMut) -> Result<&mut ArrayOfTables, String> {
    if doc.get("machine").is_none() {
        doc.insert("machine", Item::ArrayOfTables(ArrayOfTables::new()));
    }
    doc.get_mut("machine")
        .and_then(Item::as_array_of_tables_mut)
        .ok_or_else(|| "config.toml defines `machine` in a form Muxen can't edit. Use [[machine]] tables.".to_string())
}

fn table_name(t: &Table) -> &str {
    t.get("name").and_then(Item::as_str).map(str::trim).unwrap_or("")
}

/// Index of the table whose id matches, replaying `Inventory::parse`'s id rules
/// (unnamed tables are skipped and get no id).
fn find_machine(machines: &ArrayOfTables, machine_id: &str) -> Result<usize, String> {
    let mut taken = HashSet::new();
    for (i, t) in machines.iter().enumerate() {
        let name = table_name(t);
        if name.is_empty() {
            continue;
        }
        let explicit = t.get("id").and_then(Item::as_str).unwrap_or("");
        if assign_machine_id(name, explicit, &mut taken) == machine_id {
            return Ok(i);
        }
    }
    Err(format!("Machine \"{machine_id}\" isn't in config.toml."))
}

#[cfg(test)]
mod workspace_tests {
    use super::*;

    #[test]
    fn toggles_workspaces_keeping_comments() {
        let text = "# fleet\n[[machine]]\nname = \"Box\" # main box\nhost = \"box\"\n";
        let off = set_workspaces(text, "box", false).unwrap();
        assert!(off.contains("# main box") && off.contains("workspaces = false"), "{off}");
        assert!(!Inventory::parse(&off).unwrap().machines[0].workspaces);
        let on = set_workspaces(&off, "box", true).unwrap();
        assert!(!on.contains("workspaces"), "{on}");
        assert!(Inventory::parse(&on).unwrap().machines[0].workspaces);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn cloud_machines_keep_where_they_live() {
        let (text, id) = super::add_machine("# my fleet\n", "Droplet", "me@203.0.113.7").unwrap();
        let cloud = crate::config::CloudRef {
            provider: "digitalocean".into(),
            id: 42,
            region: "nyc3".into(),
            size: "s-2vcpu-4gb".into(),
            price_hourly: 0.03571,
            created_at: "2026-10-09T12:00:00Z".into(),
        };
        let text = super::set_machine_cloud(&text, &id, Some(&cloud)).unwrap();
        assert!(text.starts_with("# my fleet"));
        let inv = crate::config::Inventory::parse(&text).unwrap();
        assert_eq!(inv.machines[0].cloud.as_ref(), Some(&cloud));
        let text = super::set_machine_cloud(&text, &id, None).unwrap();
        assert!(crate::config::Inventory::parse(&text).unwrap().machines[0].cloud.is_none());
    }

    use super::*;
    use crate::config::default_inventory;

    const FILE: &str = r#"# My fleet
# (hand written)

[[machine]]
name = "Mac Studio"   # the big one
host = "ci@studio"
runners = ["studio-01", "studio-02"]

# The NAS doesn't run jobs.
[[machine]]
name = "NAS"
host = "nas"
"#;

    #[test]
    fn adds_a_machine_and_keeps_comments() {
        let (out, id) = add_machine(FILE, "Beelink", "beelink").unwrap();
        assert_eq!(id, "beelink");
        assert!(out.starts_with("# My fleet\n# (hand written)\n"));
        assert!(out.contains("name = \"Mac Studio\"   # the big one"));
        assert!(out.contains("# The NAS doesn't run jobs."));
        assert!(out.ends_with("[[machine]]\nname = \"Beelink\"\nhost = \"beelink\"\n"), "{out}");
        assert_eq!(Inventory::parse(&out).unwrap().machines.len(), 3);
        assert!(add_machine(&out, "beelink", "x").is_err());
    }

    #[test]
    fn adds_to_the_default_inventory_and_an_empty_file() {
        let (out, id) = add_machine(&default_inventory(), "Beelink", "beelink").unwrap();
        assert_eq!(id, "beelink");
        // The local machine's commented-out runners line stays with it, the examples stay last.
        let local = out.find("host = \"local\"").unwrap();
        let commented = out.find("# runners = [").unwrap();
        let beelink = out.find("name = \"Beelink\"").unwrap();
        let examples = out.find("# name = \"Mac Studio\"").unwrap();
        assert!(local < commented && commented < beelink && beelink < examples, "{out}");
        assert!(out.contains("host = \"local\"\n# runners = ["), "{out}");
        assert!(out.contains("]\n\n[[machine]]\nname = \"Beelink\"\nhost = \"beelink\"\n\n# [[machine]]"), "{out}");
        assert_eq!(Inventory::parse(&out).unwrap().machines.len(), 2);
        let (out, id) = add_machine("", "First", "local").unwrap();
        assert_eq!(id, "first");
        assert!(Inventory::parse(&out).unwrap().machines[0].is_local());
    }

    #[test]
    fn duplicate_names_get_the_same_ids_as_the_parser() {
        let text = "[[machine]]\nname = \"Box\"\nhost = \"a\"\n\n[[machine]]\nname = \"Box\"\nhost = \"b\"\n";
        let out = add_runners(text, "box-2", &["b-01".into()]).unwrap();
        let inv = Inventory::parse(&out).unwrap();
        assert!(inv.machines[0].runners.is_empty());
        assert_eq!(inv.machines[1].runners, vec!["b-01"]);
    }

    #[test]
    fn adds_and_removes_runners() {
        let out = add_runners(FILE, "mac-studio", &["studio-03".into(), "STUDIO-01".into()]).unwrap();
        assert!(out.contains(r#"runners = ["studio-01", "studio-02", "studio-03"]"#), "{out}");
        let out = add_runners(&out, "nas", &["nas-01".into()]).unwrap();
        assert_eq!(Inventory::parse(&out).unwrap().machines[1].runners, vec!["nas-01"]);
        let out = remove_runner(&out, "mac-studio", "studio-02").unwrap();
        assert_eq!(Inventory::parse(&out).unwrap().machines[0].runners, vec!["studio-01", "studio-03"]);
        assert!(out.contains("# the big one"));
    }

    #[test]
    fn removes_a_machine() {
        let out = remove_machine(FILE, "mac-studio").unwrap();
        let inv = Inventory::parse(&out).unwrap();
        assert_eq!(inv.machines.len(), 1);
        assert_eq!(inv.machines[0].name, "NAS");
        assert!(out.starts_with("# My fleet\n# (hand written)\n\n# The NAS doesn't run jobs.\n[[machine]]"), "{out}");
        assert!(!out.contains("Mac Studio"));
        assert!(remove_machine(FILE, "nope").is_err());

        // Removing the last machine keeps the header too.
        let out = remove_machine(&out, "nas").unwrap();
        assert!(out.starts_with("# My fleet\n# (hand written)\n"), "{out}");
        assert!(Inventory::parse(&out).unwrap().machines.is_empty());

        // Removing a later machine takes its own comment with it.
        let out = remove_machine(FILE, "nas").unwrap();
        assert!(!out.contains("The NAS"));
        assert!(out.starts_with("# My fleet"));
    }

    #[test]
    fn renames_a_machine_and_keeps_its_id() {
        let out = rename_machine(FILE, "mac-studio", "Studio").unwrap();
        assert!(out.contains("name = \"Studio\"   # the big one"), "{out}");
        let inv = Inventory::parse(&out).unwrap();
        assert_eq!((inv.machines[0].id.as_str(), inv.machines[0].name.as_str()), ("mac-studio", "Studio"));
        assert_eq!(inv.machines[0].runners, vec!["studio-01", "studio-02"]);
        assert!(out.contains("id = \"mac-studio\""), "{out}");

        // A name that gives the same id needs no `id` line, and case-only changes are fine.
        let out = rename_machine(FILE, "nas", "nas").unwrap();
        assert!(!out.contains("id ="), "{out}");
        assert_eq!(Inventory::parse(&out).unwrap().machines[1].name, "nas");

        // Renaming again keeps the id it already pinned.
        let again = rename_machine(&rename_machine(FILE, "mac-studio", "Studio").unwrap(), "mac-studio", "Big Mac").unwrap();
        let inv = Inventory::parse(&again).unwrap();
        assert_eq!((inv.machines[0].id.as_str(), inv.machines[0].name.as_str()), ("mac-studio", "Big Mac"));
        assert_eq!(again.matches("id = ").count(), 1, "{again}");

        assert!(rename_machine(FILE, "mac-studio", "NAS").is_err());
        assert!(rename_machine(FILE, "nope", "X").is_err());
    }

    #[test]
    fn renaming_a_repeated_name_keeps_its_numbered_id() {
        let text = "[[machine]]\nname = \"Box\"\nhost = \"a\"\n\n[[machine]]\nname = \"Box\"\nhost = \"b\"\n";
        let out = rename_machine(text, "box-2", "Spare").unwrap();
        let inv = Inventory::parse(&out).unwrap();
        assert_eq!(inv.machines.iter().map(|m| (m.id.as_str(), m.name.as_str())).collect::<Vec<_>>(), [("box", "Box"), ("box-2", "Spare")]);
    }

    #[test]
    fn refuses_broken_files() {
        assert!(add_machine("[[machine]\nname=", "x", "y").is_err());
        assert!(add_machine("machine = 3\n", "x", "y").is_err());
    }
}
