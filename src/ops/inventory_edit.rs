//! Edits config.toml in place with `toml_edit`, so the user's comments, ordering
//! and formatting survive when Muxin adds or removes machines and runners.
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
    add_machine_at(text, name, host, None)
}

/// The same, with the ssh port when it isn't 22 (no ssh config says it: an iPad).
pub fn add_machine_at(text: &str, name: &str, host: &str, port: Option<u16>) -> Result<(String, String), String> {
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
        if let Some(port) = port.filter(|p| *p != 22) {
            table["port"] = value(i64::from(port));
        }
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

/// Makes a machine one Muxin created as a VM: how ssh reaches it (through its
/// host, to the port its container publishes) and what it is.
pub fn set_machine_vm(text: &str, machine_id: &str, port: u16, user: &str, ssh_options: &[String], direct_address: Option<&str>, vm: &crate::config::VmRef) -> Result<String, String> {
    let mut doc = parse(text)?;
    let machines = machines_mut(&mut doc)?;
    let index = find_machine(machines, machine_id)?;
    let table = machines.get_mut(index).ok_or("That machine isn't in config.toml.")?;
    table["user"] = value(user);
    table["port"] = value(port as i64);
    if !ssh_options.is_empty() {
        let mut list = toml_edit::Array::new();
        for o in ssh_options {
            list.push(o.as_str());
        }
        table["ssh_options"] = value(list);
    }
    if let Some(a) = direct_address {
        table["direct_address"] = value(a);
    }
    let mut t = toml_edit::InlineTable::new();
    t.insert("host", vm.host.as_str().into());
    t.insert("os", vm.os.as_str().into());
    t.insert("version", vm.version.as_str().into());
    t.insert("container", vm.container.as_str().into());
    if let Some(p) = vm.web_port {
        t.insert("web_port", (p as i64).into());
    }
    t.insert("created_at", vm.created_at.as_str().into());
    table["vm"] = value(t);
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

/// Disconnects a machine (`disconnected = true`) or connects it again
/// (connected is the default, so the key goes).
pub fn set_disconnected(text: &str, machine_id: &str, disconnected: bool) -> Result<String, String> {
    let mut doc = parse(text)?;
    let machines = machines_mut(&mut doc)?;
    let index = find_machine(machines, machine_id)?;
    let table = machines.get_mut(index).ok_or("Machine not found.")?;
    if disconnected {
        table["disconnected"] = value(true);
    } else {
        table.remove("disconnected");
    }
    finish(doc)
}

/// Stops showing a warning (by its code) for a machine, or shows it again.
/// The list goes when it's empty.
pub fn set_warning_ignored(text: &str, machine_id: &str, code: &str, ignored: bool) -> Result<String, String> {
    let mut doc = parse(text)?;
    let machines = machines_mut(&mut doc)?;
    let index = find_machine(machines, machine_id)?;
    let table = machines.get_mut(index).ok_or("Machine not found.")?;
    let mut codes: Vec<String> = table.get("ignore_warnings").and_then(Item::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
    codes.retain(|c| c != code);
    if ignored {
        codes.push(code.to_string());
    }
    if codes.is_empty() {
        table.remove("ignore_warnings");
    } else {
        table["ignore_warnings"] = value(codes.into_iter().collect::<Array>());
    }
    finish(doc)
}

/// Puts the `[[machine]]` tables in this order, by id (tables it doesn't
/// name keep their order, after it). Comments above a machine go with it;
/// the file's header stays at the top, and lines right under a table (a
/// commented-out `# runners = [...]`) stay with that table. Ids don't
/// change: a machine whose id came from its place (the second "Box" is
/// "box-2") gets it written as `id`.
pub fn reorder_machines(text: &str, ids: &[String]) -> Result<String, String> {
    let mut doc = parse(text)?;
    let trailing = doc.trailing().as_str().unwrap_or("").to_string();
    let machines = machines_mut(&mut doc)?;
    let tables: Vec<Table> = machines.iter().cloned().collect();
    let n = tables.len();
    let id_of = |t: &Table, taken: &mut HashSet<String>| {
        let name = table_name(t);
        (!name.is_empty()).then(|| assign_machine_id(name, t.get("id").and_then(Item::as_str).unwrap_or(""), taken))
    };
    let mut taken = HashSet::new();
    let old_ids: Vec<Option<String>> = tables.iter().map(|t| id_of(t, &mut taken)).collect();
    let rank = |i: usize| old_ids[i].as_ref().and_then(|id| ids.iter().position(|x| x == id)).unwrap_or(usize::MAX);
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by_key(|&i| rank(i));
    if order.iter().enumerate().all(|(k, &i)| k == i) {
        return Ok(text.to_string());
    }

    // What's above each table: the first one's starts with the file's header
    // (up to its last blank line), which stays first; a later one's starts
    // with lines that belong to the table above it (up to its first blank line).
    let prefix = |t: &Table| t.decor().prefix().and_then(|p| p.as_str()).unwrap_or("").to_string();
    let first = prefix(&tables[0]);
    let header = file_header(&first).to_string();
    let mut own = vec![String::new(); n];
    let mut below = vec![String::new(); n];
    own[0] = first[header.len()..].to_string();
    for i in 1..n {
        let p = prefix(&tables[i]);
        let (above, mine) = split_above(&p);
        below[i - 1] = above.to_string();
        own[i] = mine.to_string();
    }
    let (last_below, rest) = split_attached(&trailing);
    below[n - 1] = last_below.to_string();

    // Each table takes the k-th machine's place in the file, its sub-tables
    // (`[machine.cloud]`) with it.
    let places: Vec<Option<isize>> = tables.iter().map(Table::position).collect();
    fn place(t: &mut Table, position: isize) {
        t.set_position(Some(position));
        for (_, item) in t.iter_mut() {
            if let Some(sub) = item.as_table_mut() {
                place(sub, position);
            }
        }
    }
    let mut fresh = ArrayOfTables::new();
    for (k, &i) in order.iter().enumerate() {
        let mut t = tables[i].clone();
        let p = if k == 0 {
            // A blank line above it (after [ui]) when the first one had one.
            let gap = if header.is_empty() && first.starts_with('\n') { "\n" } else { "" };
            format!("{header}{gap}{}", own[i].trim_start_matches('\n'))
        } else {
            // The old first table had its blank line in the header.
            let mine = if i == 0 && !own[i].starts_with('\n') { format!("\n{}", own[i]) } else { own[i].clone() };
            format!("{}{mine}", below[order[k - 1]])
        };
        t.decor_mut().set_prefix(p);
        if let Some(position) = places[k] {
            place(&mut t, position);
        }
        fresh.push(t);
    }
    *machines = fresh;
    doc.set_trailing(format!("{}{rest}", below[order[n - 1]]));

    // Ids from place ("box-2") would follow the new place: write them down.
    let new_ids: Vec<Option<String>> = {
        let mut taken = HashSet::new();
        order.iter().map(|&i| id_of(&tables[i], &mut taken)).collect()
    };
    if order.iter().zip(&new_ids).any(|(&i, id)| &old_ids[i] != id) {
        let machines = machines_mut(&mut doc)?;
        for (k, &i) in order.iter().enumerate() {
            let Some(id) = &old_ids[i] else { continue };
            if id_of(&tables[i], &mut HashSet::new()).as_ref() != Some(id) {
                if let Some(t) = machines.get_mut(k) {
                    t["id"] = value(id.as_str());
                }
            }
        }
    }
    let out = finish(doc)?;
    let kept: Vec<String> = Inventory::parse(&out).map_err(|e| e.to_string())?.machines.into_iter().map(|m| m.id).collect();
    let want: Vec<String> = order.iter().filter_map(|&i| old_ids[i].clone()).collect();
    if kept != want {
        return Err("Moving it would change a machine's id, so the order wasn't changed.".into());
    }
    Ok(out)
}

/// A later table's prefix: the lines that belong to the table above it
/// (right under it, up to a blank line), and its own (from that blank line on).
/// With no blank line, they're all its own.
fn split_above(prefix: &str) -> (&str, &str) {
    match prefix.find("\n\n") {
        Some(i) if !prefix.starts_with('\n') => (&prefix[..=i], &prefix[i + 1..]),
        _ => ("", prefix),
    }
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
        .ok_or_else(|| "config.toml defines `machine` in a form Muxin can't edit. Use [[machine]] tables.".to_string())
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

    #[test]
    fn disconnects_and_connects() {
        let text = "[[machine]]\nname = \"Box\" # main box\nhost = \"box\"\n";
        let off = set_disconnected(text, "box", true).unwrap();
        assert!(off.contains("# main box") && off.contains("disconnected = true"), "{off}");
        let m = &Inventory::parse(&off).unwrap().machines[0];
        assert!(m.disconnected && m.is_checkable() && !m.is_connected());
        assert!(m.refuse_if_disconnected().unwrap_err().contains("Box is disconnected"));
        let on = set_disconnected(&off, "box", false).unwrap();
        assert!(!on.contains("disconnected"), "{on}");
        assert!(Inventory::parse(&on).unwrap().machines[0].is_connected());
    }

    #[test]
    fn ignores_warnings_and_shows_them_again() {
        let text = "[[machine]]\nname = \"NAS\" # the qnap\nhost = \"nas\"\n";
        let one = set_warning_ignored(text, "nas", "low-disk", true).unwrap();
        assert!(one.contains("# the qnap") && one.contains("ignore_warnings = [\"low-disk\"]"), "{one}");
        let two = set_warning_ignored(&one, "nas", "reboot-required", true).unwrap();
        let again = set_warning_ignored(&two, "nas", "low-disk", true).unwrap();
        assert_eq!(Inventory::parse(&again).unwrap().machines[0].ignore_warnings, ["reboot-required", "low-disk"]);
        let back = set_warning_ignored(&set_warning_ignored(&again, "nas", "low-disk", false).unwrap(), "nas", "reboot-required", false).unwrap();
        assert!(!back.contains("ignore_warnings"), "{back}");
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
        let examples = out.find("# name = \"Forge\"").unwrap();
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

    fn order_of(text: &str) -> Vec<String> {
        Inventory::parse(text).unwrap().machines.into_iter().map(|m| m.id).collect()
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn reorders_machines_keeping_comments_and_the_header() {
        let out = reorder_machines(FILE, &ids(&["nas", "mac-studio"])).unwrap();
        assert_eq!(order_of(&out), ["nas", "mac-studio"]);
        // The header stays on top; each machine's comments go with it.
        assert!(out.starts_with("# My fleet\n# (hand written)\n\n# The NAS doesn't run jobs.\n[[machine]]\nname = \"NAS\""), "{out}");
        assert!(out.contains("host = \"nas\"\n\n[[machine]]\nname = \"Mac Studio\"   # the big one"), "{out}");
        assert_eq!(Inventory::parse(&out).unwrap().machines[1].runners, vec!["studio-01", "studio-02"]);
        // And back: the file as it was.
        assert_eq!(reorder_machines(&out, &ids(&["mac-studio", "nas"])).unwrap(), FILE);
        // The same order changes nothing.
        assert_eq!(reorder_machines(FILE, &ids(&["mac-studio", "nas"])).unwrap(), FILE);
    }

    #[test]
    fn reorders_after_settings_and_keeps_lines_under_their_table() {
        // As Muxin writes it: [settings] and [ui] first, then the inventory's
        // header, a commented-out runners line under the first machine, and
        // examples at the end.
        let (text, _) = add_machine(&default_inventory(), "Beelink", "beelink").unwrap();
        let ui: toml::Table = "mode = \"system\"".parse().unwrap();
        let text = crate::config::set_section(&text, "ui", &ui).unwrap();
        let (text, _) = add_machine(&text, "Work", "work").unwrap();
        let local = order_of(&text)[0].clone();
        let out = reorder_machines(&text, &ids(&["work", "beelink", &local])).unwrap();
        assert_eq!(order_of(&out), ["work", "beelink", local.as_str()]);
        // [ui] and the header stay first; the runners line stays under the local machine; examples last.
        let at = |s: &str| out.find(s).unwrap_or_else(|| panic!("{s} isn't in:\n{out}"));
        assert!(at("[ui]") < at("# Each [[machine]] is a computer") && at("# Each [[machine]] is a computer") < at("name = \"Work\""), "{out}");
        assert!(at("name = \"Work\"") < at("name = \"Beelink\"") && at("name = \"Beelink\"") < at("host = \"local\"\n#"), "{out}");
        assert!(out.contains("host = \"local\"\n# runners = ["), "{out}");
        assert!(at("# runners = [") < at("# name = \"Forge\""), "{out}");
        // A blank line between machines, none doubled.
        assert!(out.contains("host = \"work\"\n\n[[machine]]\nname = \"Beelink\""), "{out}");
        assert!(out.contains("host = \"beelink\"\n\n[[machine]]\nname = "), "{out}");
        assert!(!out.contains("\n\n\n"), "{out}");
        // Back to where it was.
        assert_eq!(reorder_machines(&out, &ids(&[&local, "beelink", "work"])).unwrap(), text);
    }

    #[test]
    fn reordering_keeps_numbered_ids() {
        let text = "[[machine]]\nname = \"Box\"\nhost = \"a\"\n\n[[machine]]\nname = \"Box\"\nhost = \"b\"\n\n[[machine]]\nname = \"NAS\"\nhost = \"nas\"\n";
        let out = reorder_machines(text, &ids(&["nas", "box-2", "box"])).unwrap();
        let inv = Inventory::parse(&out).unwrap();
        assert_eq!(inv.machines.iter().map(|m| (m.id.as_str(), m.host.as_str())).collect::<Vec<_>>(), [("nas", "nas"), ("box-2", "b"), ("box", "a")]);
        // Only the one whose id came from its place gets it written.
        assert_eq!(out.matches("id = ").count(), 1, "{out}");
    }

    #[test]
    fn reordering_moves_sub_tables_and_leaves_out_nothing() {
        let text = "[[machine]]\nname = \"Droplet\"\nhost = \"d\"\n[machine.cloud]\nprovider = \"digitalocean\"\nid = 7\nregion = \"nyc3\"\nsize = \"s\"\nprice_hourly = 0.01\ncreated_at = \"2026-11-09T00:00:00Z\"\n\n[[machine]]\nname = \"A\"\nhost = \"a\"\n\n[[machine]]\nname = \"B\"\nhost = \"b\"\n";
        // Ids it doesn't name keep their order, after the rest; unknown ones are ignored.
        let out = reorder_machines(text, &ids(&["b", "gone"])).unwrap();
        assert_eq!(order_of(&out), ["b", "droplet", "a"]);
        let inv = Inventory::parse(&out).unwrap();
        assert_eq!(inv.machines[1].cloud.as_ref().map(|c| c.id), Some(7));
        assert!(inv.machines[0].cloud.is_none() && inv.machines[2].cloud.is_none());
        assert!(out.find("[machine.cloud]").unwrap() < out.find("name = \"A\"").unwrap(), "{out}");
        // An empty file has nothing to move.
        assert_eq!(reorder_machines("", &ids(&["x"])).unwrap(), "");
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
