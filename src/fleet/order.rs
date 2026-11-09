//! The order machines are listed in: the sidebar's (so ⌘1, ⌘2, …), the
//! Overview's and the palette's. Each machine is followed by the machines
//! that run on it (its VMs and containers), which stay under it.
//!
//! Until the user puts them in order (drags one, or Move Up, Move Down, Move
//! to Top), favorites come first, then machines with runners, then the rest.
//! After that it's the order of the `[[machine]]` tables in config.toml,
//! which a move rewrites, so `muxin status` and the MCP tools list them the
//! same way.

/// A machine, as far as its place goes.
#[derive(Debug, Clone)]
pub struct Entry {
    pub id: String,
    /// The machine it runs on, when that one is in Muxin too: it's listed under it.
    pub host: Option<String>,
    pub favorite: bool,
    pub runners: bool,
}

/// How a machine moves. It stays at its level: among the machines with no
/// host, or among those on the same host. The machines on it go with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Move {
    Up,
    Down,
    Top,
    /// Into this machine's place (it was dropped on it); the others there
    /// shift by one. It must be at the same level.
    Onto(String),
}

/// The order to list them in, as indexes into `entries` (config.toml's
/// order). `sorted`: favorites first, then machines with runners (before the
/// user has ordered them).
pub fn arrange(entries: &[Entry], sorted: bool) -> Vec<usize> {
    let mut by: Vec<usize> = (0..entries.len()).collect();
    if sorted {
        by.sort_by_key(|&i| (!entries[i].favorite, !entries[i].runners));
    }
    let mut placed = vec![false; entries.len()];
    let mut out = Vec::with_capacity(entries.len());
    fn place(i: usize, entries: &[Entry], by: &[usize], placed: &mut [bool], out: &mut Vec<usize>) {
        if placed[i] {
            return;
        }
        placed[i] = true;
        out.push(i);
        for &c in by.iter().filter(|&&c| host_of(entries, c) == Some(i)) {
            place(c, entries, by, placed, out);
        }
    }
    for &i in by.iter().filter(|&&i| host_of(entries, i).is_none()) {
        place(i, entries, &by, &mut placed, &mut out);
    }
    // Anything left (machines on each other, which can't be): at the end.
    for &i in &by {
        if !placed[i] {
            placed[i] = true;
            out.push(i);
        }
    }
    out
}

/// Every machine's id in the order after the move, or None when it doesn't
/// move (already first or last, not at the target's level, unknown).
pub fn moved(entries: &[Entry], sorted: bool, id: &str, how: &Move) -> Option<Vec<String>> {
    let order = arrange(entries, sorted);
    let me = entries.iter().position(|e| e.id == id)?;
    let level = host_of(entries, me);
    let mut siblings: Vec<usize> = order.iter().copied().filter(|&i| host_of(entries, i) == level).collect();
    let at = siblings.iter().position(|&i| i == me)?;
    let to = match how {
        Move::Up => at.checked_sub(1)?,
        Move::Down => at + 1,
        Move::Top => 0,
        Move::Onto(target) => {
            let t = entries.iter().position(|e| &e.id == target)?;
            if host_of(entries, t) != level {
                return None;
            }
            siblings.iter().position(|&i| i == t)?
        }
    };
    if to == at || to >= siblings.len() {
        return None;
    }
    siblings.remove(at);
    siblings.insert(to, me);

    // The whole list again, each machine followed by the ones on it, with
    // this level in its new order.
    let mut kids: Vec<Vec<usize>> = vec![Vec::new(); entries.len()];
    let mut roots = Vec::new();
    for &i in &order {
        match host_of(entries, i) {
            Some(h) => kids[h].push(i),
            None => roots.push(i),
        }
    }
    match level {
        Some(h) => kids[h] = siblings,
        None => roots = siblings,
    }
    let mut placed = vec![false; entries.len()];
    let mut out = Vec::with_capacity(entries.len());
    fn walk(i: usize, kids: &[Vec<usize>], placed: &mut [bool], out: &mut Vec<usize>) {
        if placed[i] {
            return;
        }
        placed[i] = true;
        out.push(i);
        for &c in &kids[i] {
            walk(c, kids, placed, out);
        }
    }
    for &i in &roots {
        walk(i, &kids, &mut placed, &mut out);
    }
    for &i in &order {
        if !placed[i] {
            placed[i] = true;
            out.push(i);
        }
    }
    Some(out.into_iter().map(|i| entries[i].id.clone()).collect())
}

/// Where a machine dropped on `target` lands (`Move::Onto`): after it and the
/// machines on it when it comes from above, before it from below. None when
/// the drop does nothing (itself, another level, unknown).
pub fn lands_after(entries: &[Entry], sorted: bool, id: &str, target: &str) -> Option<bool> {
    let me = entries.iter().position(|e| e.id == id)?;
    let t = entries.iter().position(|e| e.id == target)?;
    if me == t || host_of(entries, me) != host_of(entries, t) {
        return None;
    }
    let order = arrange(entries, sorted);
    Some(order.iter().position(|&i| i == me)? < order.iter().position(|&i| i == t)?)
}

/// The machine `i` is listed under: its host, when that's another of `entries`.
fn host_of(entries: &[Entry], i: usize) -> Option<usize> {
    let h = entries[i].host.as_deref()?;
    entries.iter().position(|e| e.id == h).filter(|&p| p != i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, host: Option<&str>) -> Entry {
        Entry { id: id.into(), host: host.map(Into::into), favorite: false, runners: false }
    }

    /// The machines in the screenshot: Win11 is a VM on Beelink, which has a runner.
    fn fleet() -> Vec<Entry> {
        let mut beelink = entry("beelink", None);
        beelink.runners = true;
        vec![entry("personal", None), beelink, entry("work", None), entry("win11", Some("beelink")), entry("webtech", None)]
    }

    fn ids(entries: &[Entry], order: &[usize]) -> Vec<String> {
        order.iter().map(|&i| entries[i].id.clone()).collect()
    }

    #[test]
    fn sorted_puts_favorites_then_runners_first_and_vms_under_their_host() {
        let mut f = fleet();
        assert_eq!(ids(&f, &arrange(&f, true)), ["beelink", "win11", "personal", "work", "webtech"]);
        f[4].favorite = true;
        assert_eq!(ids(&f, &arrange(&f, true)), ["webtech", "beelink", "win11", "personal", "work"]);
    }

    #[test]
    fn ordered_is_config_order_with_vms_under_their_host() {
        let f = fleet();
        assert_eq!(ids(&f, &arrange(&f, false)), ["personal", "beelink", "win11", "work", "webtech"]);
    }

    #[test]
    fn a_host_that_isnt_listed_or_a_loop_doesnt_lose_machines() {
        let f = vec![entry("a", Some("b")), entry("b", Some("a")), entry("c", Some("gone")), entry("d", Some("d"))];
        // c and d have no host here: they're top-level; a and b (on each other) go last.
        assert_eq!(ids(&f, &arrange(&f, false)), ["c", "d", "a", "b"]);
    }

    #[test]
    fn moves_up_down_and_to_the_top_from_the_order_shown() {
        let f = fleet();
        // Shown (sorted): beelink, win11, personal, work, webtech.
        assert_eq!(moved(&f, true, "work", &Move::Up).unwrap(), ["beelink", "win11", "work", "personal", "webtech"]);
        assert_eq!(moved(&f, true, "personal", &Move::Down).unwrap(), ["beelink", "win11", "work", "personal", "webtech"]);
        assert_eq!(moved(&f, true, "webtech", &Move::Top).unwrap(), ["webtech", "beelink", "win11", "personal", "work"]);
        // Ordered: personal, beelink, win11, work, webtech.
        assert_eq!(moved(&f, false, "webtech", &Move::Up).unwrap(), ["personal", "beelink", "win11", "webtech", "work"]);
    }

    #[test]
    fn a_host_takes_its_vms_along() {
        let f = fleet();
        assert_eq!(moved(&f, false, "beelink", &Move::Up).unwrap(), ["beelink", "win11", "personal", "work", "webtech"]);
        assert_eq!(moved(&f, false, "beelink", &Move::Down).unwrap(), ["personal", "work", "beelink", "win11", "webtech"]);
        // Moving past a host goes past its VMs too.
        assert_eq!(moved(&f, false, "personal", &Move::Down).unwrap(), ["beelink", "win11", "personal", "work", "webtech"]);
    }

    #[test]
    fn nothing_moves_past_the_ends() {
        let f = fleet();
        assert_eq!(moved(&f, false, "personal", &Move::Up), None);
        assert_eq!(moved(&f, false, "personal", &Move::Top), None);
        assert_eq!(moved(&f, false, "webtech", &Move::Down), None);
        assert_eq!(moved(&f, false, "nope", &Move::Up), None);
    }

    #[test]
    fn a_vm_moves_only_among_the_machines_on_its_host() {
        let mut f = fleet();
        f.push(entry("ubuntu", Some("beelink")));
        f.push(entry("alpine", Some("work")));
        // Under beelink: win11, ubuntu.
        assert_eq!(moved(&f, false, "ubuntu", &Move::Up).unwrap(), ["personal", "beelink", "ubuntu", "win11", "work", "alpine", "webtech"]);
        assert_eq!(moved(&f, false, "ubuntu", &Move::Top).unwrap(), ["personal", "beelink", "ubuntu", "win11", "work", "alpine", "webtech"]);
        // It doesn't leave beelink, at either end.
        assert_eq!(moved(&f, false, "win11", &Move::Up), None);
        assert_eq!(moved(&f, false, "ubuntu", &Move::Down), None);
        // Dropped on a machine at another level: nothing happens.
        assert_eq!(moved(&f, false, "ubuntu", &Move::Onto("personal".into())), None);
        assert_eq!(moved(&f, false, "ubuntu", &Move::Onto("alpine".into())), None);
        assert_eq!(moved(&f, false, "personal", &Move::Onto("win11".into())), None);
        // On a sibling: it takes its place.
        assert_eq!(moved(&f, false, "win11", &Move::Onto("ubuntu".into())).unwrap(), ["personal", "beelink", "ubuntu", "win11", "work", "alpine", "webtech"]);
    }

    #[test]
    fn dropped_on_a_machine_it_takes_its_place() {
        let f = fleet();
        // Ordered: personal, beelink(win11), work, webtech.
        // Down: it goes after the one it was dropped on.
        assert_eq!(moved(&f, false, "personal", &Move::Onto("work".into())).unwrap(), ["beelink", "win11", "work", "personal", "webtech"]);
        // Up: before it.
        assert_eq!(moved(&f, false, "webtech", &Move::Onto("beelink".into())).unwrap(), ["personal", "webtech", "beelink", "win11", "work"]);
        // On itself: nothing.
        assert_eq!(moved(&f, false, "work", &Move::Onto("work".into())), None);
    }

    #[test]
    fn the_line_is_drawn_where_a_dropped_machine_lands() {
        let mut f = fleet();
        f.push(entry("ubuntu", Some("beelink")));
        // Ordered: personal, beelink(win11, ubuntu), work, webtech.
        for (id, target) in [("personal", "work"), ("webtech", "beelink"), ("personal", "beelink"), ("win11", "ubuntu"), ("ubuntu", "win11")] {
            let after = lands_after(&f, false, id, target).unwrap();
            let order = moved(&f, false, id, &Move::Onto(target.into())).unwrap();
            let at = |x: &str| order.iter().position(|o| o == x).unwrap();
            assert_eq!(at(id) > at(target), after, "{id} on {target}: {order:?}");
        }
        assert_eq!(lands_after(&f, false, "personal", "work"), Some(true));
        assert_eq!(lands_after(&f, false, "webtech", "beelink"), Some(false));
        // Where a drop does nothing, there's no line.
        assert_eq!(lands_after(&f, false, "work", "work"), None);
        assert_eq!(lands_after(&f, false, "personal", "win11"), None);
        assert_eq!(lands_after(&f, false, "ubuntu", "work"), None);
        assert_eq!(lands_after(&f, false, "nope", "work"), None);
        // Before the user has ordered them, it's the order shown that counts:
        // beelink, win11, ubuntu, personal, work, webtech.
        assert_eq!(lands_after(&f, true, "beelink", "personal"), Some(true));
        assert_eq!(lands_after(&f, false, "beelink", "personal"), Some(false));
    }

    #[test]
    fn the_new_order_lists_the_same_way_once_ordered() {
        // What a move writes to config.toml is what the list shows afterwards.
        let f = fleet();
        for (id, how) in [("webtech", Move::Top), ("beelink", Move::Down), ("work", Move::Onto("personal".into()))] {
            let after = moved(&f, true, id, &how).unwrap();
            let reordered: Vec<Entry> = after.iter().map(|id| f.iter().find(|e| &e.id == id).unwrap().clone()).collect();
            assert_eq!(ids(&reordered, &arrange(&reordered, false)), after);
        }
    }
}
