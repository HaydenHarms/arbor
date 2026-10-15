//! Templates: saved shapes of an arrangement that the windows on a display
//! can be fitted into later.
//!
//! A template keeps the splits, their directions and sizes, and a slot for
//! each spot, noting which app was there. It doesn't keep windows: applying
//! one fits whatever is open now, loosely. Spots go to windows of the app
//! that was there first, then the rest in order. Spots left over close up;
//! windows left over join the last spot as tabs.
//!
//! It can also keep the app of a window floating in the middle of the
//! display (where Ctrl+Alt+F puts one), to put a window of it back there.

use crate::layout::{split_rects, Axis, Rect, WinId};

#[derive(Clone, Debug, PartialEq)]
pub enum Shape {
    /// A spot, and the app that was in it (lower-case, no extension), if any.
    Slot(Option<String>),
    Split { axis: Axis, children: Vec<Shape>, weights: Vec<f32> },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Template {
    pub name: String,
    pub shape: Shape,
    /// The app floating in the middle, if one was.
    pub centre: Option<String>,
}

impl Shape {
    /// Each slot's app, in reading order: the order `Layout::arrange` fills
    /// slots in.
    pub fn hints(&self) -> Vec<Option<String>> {
        let mut out = Vec::new();
        self.walk(&mut |s| out.push(s.clone()));
        out
    }

    fn walk(&self, f: &mut impl FnMut(&Option<String>)) {
        match self {
            Shape::Slot(app) => f(app),
            Shape::Split { children, .. } => children.iter().for_each(|c| c.walk(f)),
        }
    }

    /// Each slot's rect when the shape fills `area`, in reading order: for
    /// drawing a little picture of it.
    pub fn rects(&self, area: Rect) -> Vec<Rect> {
        let mut out = Vec::new();
        self.rects_into(area, &mut out);
        out
    }

    fn rects_into(&self, area: Rect, out: &mut Vec<Rect>) {
        match self {
            Shape::Slot(_) => out.push(area),
            Shape::Split { axis, children, weights } => {
                for (c, r) in children.iter().zip(split_rects(area, *axis, weights)) {
                    c.rects_into(r, out);
                }
            }
        }
    }

    /// As text, for saving: `[app]` for a slot, `h(1:...,1:...)` or
    /// `v(...)` for a split, each child after its weight.
    pub fn to_text(&self) -> String {
        match self {
            Shape::Slot(app) => format!("[{}]", app.as_deref().unwrap_or("")),
            Shape::Split { axis, children, weights } => {
                let a = if *axis == Axis::Horizontal { 'h' } else { 'v' };
                let parts: Vec<String> =
                    children.iter().zip(weights).map(|(c, w)| format!("{w}:{}", c.to_text())).collect();
                format!("{a}({})", parts.join(","))
            }
        }
    }

    /// Read back what `to_text` wrote. None if it isn't a shape.
    pub fn parse(s: &str) -> Option<Shape> {
        let (shape, rest) = parse_shape(s.trim())?;
        rest.is_empty().then_some(shape)
    }
}

fn parse_shape(s: &str) -> Option<(Shape, &str)> {
    if let Some(rest) = s.strip_prefix('[') {
        let end = rest.find(']')?;
        let app = &rest[..end];
        let app = (!app.is_empty()).then(|| app.to_string());
        return Some((Shape::Slot(app), &rest[end + 1..]));
    }
    let axis = match s.chars().next()? {
        'h' => Axis::Horizontal,
        'v' => Axis::Vertical,
        _ => return None,
    };
    let mut rest = s[1..].strip_prefix('(')?;
    let (mut children, mut weights) = (Vec::new(), Vec::new());
    loop {
        let colon = rest.find(':')?;
        let w: f32 = rest[..colon].parse().ok().filter(|w: &f32| w.is_finite() && *w > 0.0)?;
        let (child, after) = parse_shape(&rest[colon + 1..])?;
        children.push(child);
        weights.push(w);
        if let Some(after) = after.strip_prefix(',') {
            rest = after;
        } else {
            rest = after.strip_prefix(')')?;
            break;
        }
    }
    (children.len() >= 2).then_some((Shape::Split { axis, children, weights }, rest))
}

impl Template {
    /// One line of the templates file: the name, a tab, the shape, and
    /// another tab and the app in the middle if there is one.
    pub fn to_line(&self) -> String {
        let mut line = format!("{}\t{}", clean_name(&self.name), self.shape.to_text());
        if let Some(app) = &self.centre {
            line = format!("{line}\t{app}");
        }
        line
    }

    pub fn parse_line(line: &str) -> Option<Template> {
        let (name, rest) = line.split_once('\t')?;
        let name = clean_name(name);
        (!name.is_empty()).then_some(())?;
        let (shape, centre) = match rest.split_once('\t') {
            Some((shape, app)) => (shape, Some(app_key(app)).filter(|a| !a.is_empty())),
            None => (rest, None),
        };
        Some(Template { name, shape: Shape::parse(shape)?, centre })
    }
}

/// A template's name, kept to one line.
pub fn clean_name(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect::<String>().trim().to_string()
}

/// Every template in a templates file, skipping lines that don't read.
pub fn parse_file(text: &str) -> Vec<Template> {
    text.lines().filter_map(Template::parse_line).collect()
}

pub fn to_file(templates: &[Template]) -> String {
    templates.iter().map(|t| t.to_line() + "\n").collect()
}

/// An app's name for matching: its executable's name, lower-case, without
/// the extension, and nothing that would break the file format.
pub fn app_key(exe: &str) -> String {
    let name = exe.rsplit(['\\', '/']).next().unwrap_or(exe);
    let name = name.strip_suffix(".exe").or_else(|| name.strip_suffix(".EXE")).unwrap_or(name);
    name.chars().filter(|c| !matches!(c, '[' | ']' | '\t') && !c.is_control()).collect::<String>().to_lowercase()
}

/// Which window goes in each slot, given each slot's app and the windows
/// to place (in the order they should fill slots) with their apps.
/// First every slot gets a window of the app that was there, if one is
/// left; then empty slots take the remaining windows in order. Returns the
/// slots (None = nothing for it) and the windows left over.
pub fn assign(hints: &[Option<String>], wins: &[(WinId, String)]) -> (Vec<Option<WinId>>, Vec<WinId>) {
    let mut used = vec![false; wins.len()];
    let mut slots: Vec<Option<WinId>> = hints
        .iter()
        .map(|hint| {
            let hint = hint.as_ref()?;
            let i = wins.iter().enumerate().position(|(i, (_, app))| !used[i] && app == hint)?;
            used[i] = true;
            Some(wins[i].0)
        })
        .collect();
    let mut rest = wins.iter().enumerate().filter(|(i, _)| !used[*i]).map(|(_, (w, _))| *w);
    for s in slots.iter_mut().filter(|s| s.is_none()) {
        *s = rest.next();
    }
    (slots, rest.collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(app: &str) -> Shape {
        Shape::Slot((!app.is_empty()).then(|| app.to_string()))
    }

    fn sample() -> Shape {
        Shape::Split {
            axis: Axis::Horizontal,
            children: vec![
                slot("code"),
                Shape::Split { axis: Axis::Vertical, children: vec![slot("chrome"), slot("")], weights: vec![2.0, 1.0] },
            ],
            weights: vec![1.5, 1.0],
        }
    }

    #[test]
    fn text_round_trips() {
        let s = sample();
        assert_eq!(s.to_text(), "h(1.5:[code],1:v(2:[chrome],1:[]))");
        assert_eq!(Shape::parse(&s.to_text()), Some(s.clone()));
        let t = Template { name: "Work\tday\n".into(), shape: s, centre: None };
        let back = parse_file(&to_file(&[t]));
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].name, "Workday");
        assert_eq!(back[0].shape, sample());
        assert_eq!(back[0].centre, None);
    }

    #[test]
    fn the_app_in_the_middle_round_trips() {
        let t = Template { name: "Focus".into(), shape: sample(), centre: Some("windowsterminal".into()) };
        assert_eq!(t.to_line(), "Focus\th(1.5:[code],1:v(2:[chrome],1:[]))\twindowsterminal");
        assert_eq!(parse_file(&to_file(&[t.clone()])), vec![t]);
        // An empty one reads as none.
        assert_eq!(Template::parse_line("A\t[]\t").unwrap().centre, None);
    }

    #[test]
    fn bad_text_is_skipped() {
        for bad in ["", "x", "h(", "h(1:[a])", "h(0:[a],1:[b])", "h(1:[a],1:[b]", "[a]extra", "v(a:[x],1:[y])"] {
            assert_eq!(Shape::parse(bad), None, "{bad}");
        }
        assert_eq!(parse_file("no tab here\nOk\t[]\n\t[]\n").len(), 1);
    }

    #[test]
    fn hints_and_rects_in_reading_order() {
        let s = sample();
        assert_eq!(s.hints(), vec![Some("code".into()), Some("chrome".into()), None]);
        let r = s.rects(Rect::new(0, 0, 250, 300));
        assert_eq!(r, vec![Rect::new(0, 0, 150, 300), Rect::new(150, 0, 100, 200), Rect::new(150, 200, 100, 100)]);
    }

    #[test]
    fn apps_go_back_to_their_spots_then_the_rest_fill_in() {
        let hints = sample().hints();
        let wins = vec![(1, "chrome".to_string()), (2, "notepad".to_string()), (3, "code".to_string())];
        assert_eq!(assign(&hints, &wins), (vec![Some(3), Some(1), Some(2)], vec![]));
        // Too few: the last slot gets nothing.
        assert_eq!(assign(&hints, &wins[..1]), (vec![None, Some(1), None], vec![]));
        let (slots, _) = assign(&hints, &[(5, "x".into())]);
        assert_eq!(slots, vec![Some(5), None, None]);
        // Too many: the rest are left over, in order.
        let more = [&wins[..], &[(4, "x".to_string()), (5, "code".to_string())]].concat();
        assert_eq!(assign(&hints, &more), (vec![Some(3), Some(1), Some(2)], vec![4, 5]));
    }

    #[test]
    fn app_keys() {
        assert_eq!(app_key(r"C:\Program Files\Microsoft VS Code\Code.exe"), "code");
        assert_eq!(app_key("NOTEPAD.EXE"), "notepad");
        assert_eq!(app_key("we[ir]d.exe"), "weird");
    }
}
