//! Terminals split side by side or one above the other, like panes in tmux
//! or iTerm: a layout of sessions shown together on one session page. The
//! sessions themselves live on their machines; a layout only says how to
//! show them, and is saved (Appearance::panes) so it comes back after a
//! restart. Written as text: a session id, or `r(a,b)` (a left of b) and
//! `d(a,b)` (a above b).

#[derive(Clone, Debug, PartialEq)]
pub enum Panes {
    One(String),
    Split { down: bool, a: Box<Panes>, b: Box<Panes> },
}

impl Panes {
    pub fn contains(&self, id: &str) -> bool {
        match self {
            Panes::One(x) => x == id,
            Panes::Split { a, b, .. } => a.contains(id) || b.contains(id),
        }
    }

    /// The sessions, left to right and top to bottom.
    pub fn ids(&self) -> Vec<&str> {
        match self {
            Panes::One(x) => vec![x.as_str()],
            Panes::Split { a, b, .. } => {
                let mut ids = a.ids();
                ids.extend(b.ids());
                ids
            }
        }
    }

    /// Puts `new` beside `at` (right of it, or below when `down`).
    pub fn split(&mut self, at: &str, new: String, down: bool) -> bool {
        match self {
            Panes::One(x) if x == at => {
                let old = std::mem::replace(self, Panes::One(String::new()));
                *self = Panes::Split { down, a: Box::new(old), b: Box::new(Panes::One(new)) };
                true
            }
            Panes::One(_) => false,
            Panes::Split { a, b, .. } => a.split(at, new.clone(), down) || b.split(at, new, down),
        }
    }

    /// Without `id`: the pane next to it takes its place. None when nothing's left.
    pub fn without(self, id: &str) -> Option<Panes> {
        match self {
            Panes::One(x) if x == id => None,
            one @ Panes::One(_) => Some(one),
            Panes::Split { down, a, b } => match (a.without(id), b.without(id)) {
                (Some(a), Some(b)) => Some(Panes::Split { down, a: Box::new(a), b: Box::new(b) }),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            },
        }
    }

    /// The share of the width and height `id` gets (half per split above it).
    pub fn share(&self, id: &str) -> Option<(f32, f32)> {
        match self {
            Panes::One(x) => (x == id).then_some((1., 1.)),
            Panes::Split { down, a, b } => {
                let (w, h) = a.share(id).or_else(|| b.share(id))?;
                Some(if *down { (w, h / 2.) } else { (w / 2., h) })
            }
        }
    }

    pub fn encode(&self) -> String {
        match self {
            Panes::One(x) => x.clone(),
            Panes::Split { down, a, b } => format!("{}({},{})", if *down { "d" } else { "r" }, a.encode(), b.encode()),
        }
    }

    pub fn decode(text: &str) -> Option<Panes> {
        let (panes, rest) = parse(text.trim())?;
        rest.is_empty().then_some(panes)
    }
}

fn parse(text: &str) -> Option<(Panes, &str)> {
    if let Some(inner) = text.strip_prefix("r(").or_else(|| text.strip_prefix("d(")) {
        let down = text.starts_with('d');
        let (a, rest) = parse(inner)?;
        let (b, rest) = parse(rest.strip_prefix(',')?)?;
        return Some((Panes::Split { down, a: Box::new(a), b: Box::new(b) }, rest.strip_prefix(')')?));
    }
    let end = text.find([',', ')']).unwrap_or(text.len());
    let id = &text[..end];
    (!id.is_empty()).then(|| (Panes::One(id.to_string()), &text[end..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_and_closes() {
        let mut p = Panes::One("m/a".into());
        assert!(p.split("m/a", "m/b".into(), false));
        assert!(p.split("m/b", "m/c".into(), true));
        assert_eq!(p.encode(), "r(m/a,d(m/b,m/c))");
        assert_eq!(p.ids(), ["m/a", "m/b", "m/c"]);
        assert_eq!(p.share("m/a"), Some((0.5, 1.)));
        assert_eq!(p.share("m/c"), Some((0.5, 0.5)));
        assert_eq!(p.share("m/x"), None);
        assert!(!p.split("m/x", "m/y".into(), false));
        let p = p.without("m/b").unwrap();
        assert_eq!(p.encode(), "r(m/a,m/c)");
        let p = p.without("m/a").unwrap();
        assert_eq!(p, Panes::One("m/c".into()));
        assert_eq!(p.without("m/c"), None);
    }

    #[test]
    fn reads_what_it_writes() {
        for text in ["beelink/shell-ab12", "r(beelink/a,beelink/b)", "d(r(m/a,m/b),m/c-1)"] {
            assert_eq!(Panes::decode(text).map(|p| p.encode()).as_deref(), Some(text));
        }
        assert_eq!(Panes::decode("r(m/a,"), None);
        assert_eq!(Panes::decode("r(m/a,m/b))"), None);
    }
}
