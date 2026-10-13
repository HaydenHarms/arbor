//! Platform-independent layout engine.
//!
//! The desktop is a tree. Leaves are windows; internal nodes are splits that
//! lay their children out side by side (horizontal) or stacked (vertical),
//! each child taking a share of the space given by its weight. Splits nest
//! inside splits as deep as you like.
//!
//! The "camera" is a path into the tree: the node currently shown full-screen.
//! Zooming in narrows the camera to a deeper node; zooming out widens it.

pub type WinId = isize;

/// The share of a display side, at each end, where a drop stacks against
/// the window in that corner rather than taking the whole side.
const CORNER: f32 = 0.25;

/// A leaf holding free space rather than a window. When the first window
/// docks it takes half the display, and the other half is this, waiting
/// for the next window. (0 is never a real window handle.)
pub const EMPTY: WinId = 0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }
    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
    /// Linear interpolation between two rects, t in [0, 1].
    pub fn lerp(a: Rect, b: Rect, t: f32) -> Rect {
        let l = |p: i32, q: i32| p + ((q - p) as f32 * t).round() as i32;
        Rect::new(l(a.x, b.x), l(a.y, b.y), l(a.w, b.w), l(a.h, b.h))
    }
    fn inset(&self, d: i32) -> Rect {
        Rect::new(self.x + d, self.y + d, (self.w - 2 * d).max(1), (self.h - 2 * d).max(1))
    }
}

/// Ease-in-out cubic, the curve used for every layout motion.
pub fn ease(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    /// Children side by side, left to right.
    Horizontal,
    /// Children stacked, top to bottom.
    Vertical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    fn axis(self) -> Axis {
        match self {
            Edge::Left | Edge::Right => Axis::Horizontal,
            Edge::Top | Edge::Bottom => Axis::Vertical,
        }
    }
    fn before(self) -> bool {
        matches!(self, Edge::Left | Edge::Top)
    }
}

/// Where a window sat in the tree, relative to a neighbour.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    anchor: WinId,
    /// How many levels above the anchor leaf the docking node is.
    up: usize,
    edge: Edge,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Leaf(WinId),
    Split {
        axis: Axis,
        children: Vec<Node>,
        weights: Vec<f32>,
    },
}

impl Node {
    fn split(axis: Axis, children: Vec<Node>) -> Node {
        let n = children.len();
        Node::Split { axis, children, weights: vec![1.0; n] }
    }

    fn contains(&self, id: WinId) -> bool {
        match self {
            Node::Leaf(w) => *w == id,
            Node::Split { children, .. } => children.iter().any(|c| c.contains(id)),
        }
    }

    fn leaves(&self, out: &mut Vec<WinId>) {
        match self {
            Node::Leaf(w) => out.push(*w),
            Node::Split { children, .. } => children.iter().for_each(|c| c.leaves(out)),
        }
    }

    fn at_path(&self, path: &[usize]) -> Option<&Node> {
        match (path.split_first(), self) {
            (None, n) => Some(n),
            (Some((i, rest)), Node::Split { children, .. }) => children.get(*i)?.at_path(rest),
            _ => None,
        }
    }

    fn path_to(&self, id: WinId) -> Option<Vec<usize>> {
        match self {
            Node::Leaf(w) => (*w == id).then(Vec::new),
            Node::Split { children, .. } => children.iter().enumerate().find_map(|(i, c)| {
                c.path_to(id).map(|mut p| {
                    p.insert(0, i);
                    p
                })
            }),
        }
    }

    /// Lay out this subtree in `area`, appending (window, rect) pairs.
    fn layout(&self, area: Rect, gap: i32, out: &mut Vec<(WinId, Rect)>) {
        match self {
            Node::Leaf(w) => out.push((*w, area.inset(gap / 2))),
            Node::Split { axis, children, weights } => {
                for (child, r) in children.iter().zip(split_rects(area, *axis, weights)) {
                    child.layout(r, gap, out);
                }
            }
        }
    }

    /// Remove `id`; returns true if found. Collapses single-child splits.
    fn remove(&mut self, id: WinId) -> bool {
        let Node::Split { children, weights, .. } = self else { return false };
        if let Some(i) = children.iter().position(|c| matches!(c, Node::Leaf(w) if *w == id)) {
            children.remove(i);
            weights.remove(i);
        } else if !children.iter_mut().any(|c| c.remove(id)) {
            return false;
        }
        // Collapse: a split with one child becomes that child; a child split
        // with the same axis as its parent gets merged into the parent.
        if children.len() == 1 {
            *self = children.pop().unwrap();
        } else {
            self.flatten();
        }
        true
    }

    fn flatten(&mut self) {
        let Node::Split { axis, children, weights } = self else { return };
        let mut i = 0;
        while i < children.len() {
            if let Node::Split { axis: ca, .. } = &children[i] {
                if ca == axis {
                    let Node::Split { children: cc, weights: cw, .. } = children.remove(i) else {
                        unreachable!()
                    };
                    let parent_w = weights.remove(i);
                    let total: f32 = cw.iter().sum();
                    let n = cc.len();
                    for (k, (c, w)) in cc.into_iter().zip(cw).enumerate() {
                        children.insert(i + k, c);
                        weights.insert(i + k, parent_w * w / total);
                    }
                    i += n;
                    continue;
                }
            }
            i += 1;
        }
    }
}

/// Divide `area` along `axis` in proportion to `weights`, with no rounding gaps.
pub fn split_rects(area: Rect, axis: Axis, weights: &[f32]) -> Vec<Rect> {
    let total: f32 = weights.iter().sum::<f32>().max(f32::EPSILON);
    let len = match axis {
        Axis::Horizontal => area.w,
        Axis::Vertical => area.h,
    };
    let mut acc = 0.0f32;
    let mut out = Vec::with_capacity(weights.len());
    for w in weights {
        let start = (acc / total * len as f32).round() as i32;
        acc += w;
        let end = (acc / total * len as f32).round() as i32;
        out.push(match axis {
            Axis::Horizontal => Rect::new(area.x + start, area.y, end - start, area.h),
            Axis::Vertical => Rect::new(area.x, area.y + start, area.w, end - start),
        });
    }
    out
}

/// One display's arrangement: the tree, plus a camera into it.
#[derive(Clone, Debug, Default)]
pub struct Layout {
    pub root: Option<Node>,
    /// Path from the root to the node the camera is framing.
    pub camera: Vec<usize>,
    /// Gap between windows, in pixels.
    pub gap: i32,
    /// Back/forward history of camera positions.
    back: Vec<Vec<usize>>,
    forward: Vec<Vec<usize>>,
}

impl Layout {
    pub fn new(gap: i32) -> Self {
        Self { gap, ..Default::default() }
    }

    pub fn contains(&self, id: WinId) -> bool {
        id != EMPTY && self.root.as_ref().is_some_and(|r| r.contains(id))
    }

    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    /// Dock `id` against `edge`. With a `target`, it docks beside that window
    /// (nesting a new split if needed), or fills it if the target is `EMPTY`.
    /// Without one, it docks beside everything in view, filling the empty
    /// space if that's what lies against `edge`.
    pub fn dock(&mut self, id: WinId, edge: Edge, target: Option<WinId>) {
        if self.contains(id) {
            self.remove(id);
        }
        let new = Node::Leaf(id);
        let Some(root) = self.root.as_mut() else {
            // The first window takes the half of the display it was docked
            // against; the other half stays free for the next one.
            self.root = Some(if edge.before() {
                Node::split(edge.axis(), vec![new, Node::Leaf(EMPTY)])
            } else {
                Node::split(edge.axis(), vec![Node::Leaf(EMPTY), new])
            });
            self.camera.clear();
            return;
        };
        let fill = match target {
            Some(EMPTY) => root.path_to(EMPTY),
            Some(_) => None,
            None => self.empty_against(edge),
        };
        if let Some(path) = fill {
            let root = self.root.as_mut().expect("root checked above");
            *node_mut(root, &path).expect("valid empty path") = new;
            return;
        }
        let root = self.root.as_mut().expect("root checked above");
        // Find the node to dock against: the target leaf, or the camera node.
        let path = match target.and_then(|t| root.path_to(t)) {
            Some(p) => p,
            None => self.camera.clone(),
        };
        self.dock_at_path(id, edge, path);
    }

    /// Where `id` sits, as a placement that `restore` can use to put it back.
    pub fn placement_of(&self, id: WinId) -> Option<Placement> {
        let root = self.root.as_ref()?;
        let path = root.path_to(id)?;
        let (&idx, parent) = path.split_last()?;
        let Some(Node::Split { axis, children, .. }) = root.at_path(parent) else { return None };
        // Anchor to a neighbouring sibling: the one after if there is one.
        let (sib, before) = if idx + 1 < children.len() { (idx + 1, true) } else { (idx - 1, false) };
        let mut leaves = Vec::new();
        children[sib].leaves(&mut leaves);
        let anchor = *leaves.first()?;
        let anchor_depth = root.path_to(anchor)?.len();
        let edge = match (axis, before) {
            (Axis::Horizontal, true) => Edge::Left,
            (Axis::Horizontal, false) => Edge::Right,
            (Axis::Vertical, true) => Edge::Top,
            (Axis::Vertical, false) => Edge::Bottom,
        };
        Some(Placement { anchor, up: anchor_depth - path.len(), edge })
    }

    /// Put `id` back where a `placement_of` said it was, as best the tree allows.
    pub fn restore(&mut self, id: WinId, p: Placement) {
        let path = self.root.as_ref().and_then(|r| r.path_to(p.anchor)).map(|mut path| {
            path.truncate(path.len().saturating_sub(p.up));
            path
        });
        match path {
            Some(path) if !self.contains(id) => self.dock_at_path(id, p.edge, path),
            _ => self.dock(id, p.edge, None),
        }
    }

    fn dock_at_path(&mut self, id: WinId, edge: Edge, path: Vec<usize>) {
        let new = Node::Leaf(id);
        let Some(root) = self.root.as_mut() else {
            self.root = Some(new);
            return;
        };
        let axis = edge.axis();
        // If the docking node's parent already splits along this axis, become
        // a sibling instead of nesting a fresh two-way split.
        if let Some((&idx, parent_path)) = path.split_last() {
            if let Some(Node::Split { axis: pa, children, weights }) = node_mut(root, parent_path) {
                if *pa == axis {
                    let at = if edge.before() { idx } else { idx + 1 };
                    let w = weights[idx] / 2.0;
                    weights[idx] = w;
                    children.insert(at, new);
                    weights.insert(at, w);
                    return;
                }
            }
        }
        let slot = node_mut(root, &path).expect("valid dock path");
        let old = std::mem::replace(slot, Node::Leaf(0));
        *slot = if edge.before() {
            Node::split(axis, vec![new, old])
        } else {
            Node::split(axis, vec![old, new])
        };
    }

    pub fn remove(&mut self, id: WinId) -> bool {
        if id == EMPTY {
            return false;
        }
        let Some(root) = self.root.as_mut() else { return false };
        let found = if matches!(root, Node::Leaf(w) if *w == id) {
            self.root = None;
            true
        } else {
            root.remove(id)
        };
        // Empty space with no windows left is no arrangement at all.
        if matches!(self.root, Some(Node::Leaf(EMPTY))) {
            self.root = None;
        }
        if found {
            self.fix_camera();
        }
        found
    }

    /// Path to the empty space, if it fills the side of the view at `edge`.
    fn empty_against(&self, edge: Edge) -> Option<Vec<usize>> {
        let root = self.root.as_ref()?;
        let canvas = Rect::new(0, 0, 10_000, 10_000);
        let cam = self.camera_rect(canvas);
        let mut rects = Vec::new();
        root.layout(canvas, 0, &mut rects);
        let (_, r) = rects.into_iter().find(|(w, _)| *w == EMPTY)?;
        let full_side = match edge {
            Edge::Left => r.x == cam.x && r.y == cam.y && r.h == cam.h,
            Edge::Right => r.x + r.w == cam.x + cam.w && r.y == cam.y && r.h == cam.h,
            Edge::Top => r.y == cam.y && r.x == cam.x && r.w == cam.w,
            Edge::Bottom => r.y + r.h == cam.y + cam.h && r.x == cam.x && r.w == cam.w,
        };
        full_side.then(|| root.path_to(EMPTY)).flatten()
    }

    fn fix_camera(&mut self) {
        let valid = |p: &Vec<usize>, r: &Option<Node>| r.as_ref().and_then(|r| r.at_path(p)).is_some();
        while !valid(&self.camera, &self.root) && !self.camera.is_empty() {
            self.camera.pop();
        }
        self.back.retain(|p| valid(p, &self.root));
        self.forward.retain(|p| valid(p, &self.root));
    }

    /// Rects for every window, given the display's work area. Windows inside
    /// the camera fill `area`; windows outside it are laid out as if the
    /// whole tree were scaled up around the camera, so they land off-screen.
    pub fn layout(&self, area: Rect) -> Vec<(WinId, Rect)> {
        let mut out = self.layout_with_empty(area);
        out.retain(|(w, _)| *w != EMPTY);
        out
    }

    /// Like `layout`, but including the rect of the empty space, if any.
    fn layout_with_empty(&self, area: Rect) -> Vec<(WinId, Rect)> {
        let Some(root) = &self.root else { return vec![] };
        let mut out = Vec::new();
        // Lay the whole tree out in a virtual canvas, then map the camera's
        // rect onto the screen. Everything else follows the same transform.
        let canvas = Rect::new(0, 0, 10_000, 10_000);
        let mut virt = Vec::new();
        root.layout(canvas, 0, &mut virt);
        let cam = self.camera_rect(canvas);
        let sx = area.w as f32 / cam.w as f32;
        let sy = area.h as f32 / cam.h as f32;
        for (id, r) in virt {
            let mapped = Rect::new(
                area.x + ((r.x - cam.x) as f32 * sx).round() as i32,
                area.y + ((r.y - cam.y) as f32 * sy).round() as i32,
                (r.w as f32 * sx).round() as i32,
                (r.h as f32 * sy).round() as i32,
            );
            out.push((id, mapped.inset(self.gap / 2)));
        }
        out
    }

    fn camera_rect(&self, canvas: Rect) -> Rect {
        let mut node = self.root.as_ref().unwrap();
        let mut r = canvas;
        for &i in &self.camera {
            let Node::Split { axis, children, weights } = node else { break };
            r = split_rects(r, *axis, weights)[i];
            node = &children[i];
        }
        r
    }

    pub fn is_visible(&self, id: WinId) -> bool {
        self.root
            .as_ref()
            .and_then(|r| r.at_path(&self.camera))
            .is_some_and(|n| n.contains(id))
    }

    fn move_camera(&mut self, to: Vec<usize>) {
        if to != self.camera {
            self.back.push(std::mem::replace(&mut self.camera, to));
            self.forward.clear();
        }
    }

    /// Frame the smallest node containing `id` that isn't just the leaf itself:
    /// one step deeper toward the window.
    pub fn zoom_in_toward(&mut self, id: WinId) {
        let Some(path) = self.root.as_ref().filter(|_| id != EMPTY).and_then(|r| r.path_to(id)) else { return };
        if path.len() > self.camera.len() && path.starts_with(&self.camera) {
            let next = path[..self.camera.len() + 1].to_vec();
            self.move_camera(next);
        }
    }

    /// Frame a single window on its own.
    pub fn frame(&mut self, id: WinId) {
        if let Some(p) = self.root.as_ref().filter(|_| id != EMPTY).and_then(|r| r.path_to(id)) {
            self.move_camera(p);
        }
    }

    /// Step out one level.
    pub fn zoom_out(&mut self) {
        if !self.camera.is_empty() {
            let mut p = self.camera.clone();
            p.pop();
            self.move_camera(p);
        }
    }

    /// Show everything.
    pub fn zoom_all(&mut self) {
        self.move_camera(Vec::new());
    }

    /// Move the camera to the neighbouring sibling (dir = -1 or +1).
    pub fn neighbour(&mut self, dir: i32) {
        let Some((&i, parent)) = self.camera.split_last() else { return };
        let Some(Node::Split { children, .. }) = self.root.as_ref().and_then(|r| r.at_path(parent)) else {
            return;
        };
        let j = i as i32 + dir;
        // Nothing to look at in the empty space, so never frame it.
        if j >= 0 && (j as usize) < children.len() && children[j as usize] != Node::Leaf(EMPTY) {
            let mut p = parent.to_vec();
            p.push(j as usize);
            self.move_camera(p);
        }
    }

    pub fn back(&mut self) {
        if let Some(p) = self.back.pop() {
            self.forward.push(std::mem::replace(&mut self.camera, p));
        }
    }

    pub fn forward(&mut self) {
        if let Some(p) = self.forward.pop() {
            self.back.push(std::mem::replace(&mut self.camera, p));
        }
    }

    /// The user resized `id` to `new` by dragging its border. Move the seam(s)
    /// it dragged so both neighbours resize together.
    pub fn apply_resize(&mut self, id: WinId, new: Rect, area: Rect) {
        let Some(old) = self.layout(area).into_iter().find(|(w, _)| *w == id).map(|(_, r)| r) else {
            return;
        };
        let Some(root) = self.root.as_mut() else { return };
        let Some(path) = root.path_to(id) else { return };
        // Which edges moved, and by how many pixels (positive = grew).
        let moves = [
            (Edge::Left, old.x - new.x),
            (Edge::Right, (new.x + new.w) - (old.x + old.w)),
            (Edge::Top, old.y - new.y),
            (Edge::Bottom, (new.y + new.h) - (old.y + old.h)),
        ];
        // Screen pixels per unit of the virtual canvas, so a drag of N pixels
        // moves the seam N pixels on screen whatever the zoom.
        let cam = {
            let mut node = &*root;
            let mut r = Rect::new(0, 0, 10_000, 10_000);
            for &i in &self.camera {
                let Node::Split { axis, children, weights } = node else { break };
                r = split_rects(r, *axis, weights)[i];
                node = &children[i];
            }
            r
        };
        for (edge, delta) in moves {
            if delta == 0 {
                continue;
            }
            let axis = edge.axis();
            let screen_len = (if axis == Axis::Horizontal { area.w } else { area.h }) as f32;
            let cam_len = (if axis == Axis::Horizontal { cam.w } else { cam.h }) as f32;
            // Walk up from the window to the nearest split along this axis
            // that has a neighbour on the dragged side.
            for depth in (0..path.len()).rev() {
                let idx = path[depth];
                let parent = &path[..depth];
                let canvas = Rect::new(0, 0, 10_000, 10_000);
                let parent_len = {
                    let mut node = &*root;
                    let mut r = canvas;
                    for &i in parent {
                        let Node::Split { axis, children, weights } = node else { break };
                        r = split_rects(r, *axis, weights)[i];
                        node = &children[i];
                    }
                    (if axis == Axis::Horizontal { r.w } else { r.h }) as f32
                };
                let Some(Node::Split { axis: pa, weights, .. }) = node_mut(root, parent) else { continue };
                if *pa != axis {
                    continue;
                }
                let nb = if edge.before() { idx.checked_sub(1) } else { Some(idx + 1) };
                let Some(nb) = nb.filter(|&n| n < weights.len()) else { continue };
                let total: f32 = weights.iter().sum();
                let canvas_px = delta as f32 * cam_len / screen_len;
                let dw = canvas_px / parent_len * total;
                let min = total * 0.05;
                let dw = dw.min(weights[nb] - min).max(min - weights[idx]);
                weights[idx] += dw;
                weights[nb] -= dw;
                break;
            }
        }
    }

    /// Where a window dropped with the pointer at (px, py) should dock, for
    /// drag-to-dock. `exclude` is the window being dragged.
    /// Returns the window to dock beside (None for the whole view, `EMPTY`
    /// to fill the empty space) and which of its edges, or None to leave the
    /// window floating where it was dropped.
    ///
    /// Only edges dock. The middle of the display, and of every window,
    /// leaves the window floating.
    pub fn dock_target(
        &self,
        area: Rect,
        px: i32,
        py: i32,
        exclude: WinId,
    ) -> Option<(Option<WinId>, Edge)> {
        if let Some(edge) = screen_edge(area, px, py) {
            // Near either end of a side, stack against the window in that
            // corner instead of taking the whole side: top or bottom of a
            // window docked left or right, beside one docked top or bottom.
            return Some(self.corner_target(area, edge, px, py, exclude).unwrap_or((None, edge)));
        }
        for (id, r) in self.layout_with_empty(area) {
            if id == exclude || !r.contains(px, py) {
                continue;
            }
            let (reach_x, reach_y) = (edge_reach(r.w), edge_reach(r.h));
            let edges = [
                (Edge::Left, px - r.x, reach_x),
                (Edge::Right, r.x + r.w - 1 - px, reach_x),
                (Edge::Top, py - r.y, reach_y),
                (Edge::Bottom, r.y + r.h - 1 - py, reach_y),
            ];
            let (edge, ..) = edges
                .into_iter()
                .filter(|&(_, d, reach)| d < reach)
                .min_by(|a, b| (a.1 as f32 / a.2 as f32).total_cmp(&(b.1 as f32 / b.2 as f32)))?;
            if id == EMPTY {
                return Some((Some(EMPTY), self.empty_side(area, r)));
            }
            return Some((Some(id), edge));
        }
        None
    }

    /// For a drop at the display's `edge` near one end of that side, the
    /// window in that corner and the edge of it to stack against.
    fn corner_target(&self, area: Rect, edge: Edge, px: i32, py: i32, exclude: WinId) -> Option<(Option<WinId>, Edge)> {
        // Where along the side the pointer is, 0..1, and the point on the
        // side to look for a window at.
        let (along, x, y) = match edge {
            Edge::Left => ((py - area.y) as f32 / area.h as f32, area.x + 1, py),
            Edge::Right => ((py - area.y) as f32 / area.h as f32, area.x + area.w - 2, py),
            Edge::Top => ((px - area.x) as f32 / area.w as f32, px, area.y + 1),
            Edge::Bottom => ((px - area.x) as f32 / area.w as f32, px, area.y + area.h - 2),
        };
        let stack = match (edge.axis(), along) {
            (Axis::Horizontal, a) if a < CORNER => Edge::Top,
            (Axis::Horizontal, a) if a > 1.0 - CORNER => Edge::Bottom,
            (Axis::Vertical, a) if a < CORNER => Edge::Left,
            (Axis::Vertical, a) if a > 1.0 - CORNER => Edge::Right,
            _ => return None,
        };
        // The gaps between windows can leave the exact point uncovered, so
        // take the nearest rect there.
        let (id, r) = self
            .layout_with_empty(area)
            .into_iter()
            .filter(|(id, _)| *id != exclude)
            .min_by_key(|(_, r)| dist_to_rect(*r, x, y))?;
        // Only a window actually against that side (the point is on the
        // side's edge, so near means touching). Empty space there is filled.
        let touches = dist_to_rect(r, x, y) <= self.gap + 2;
        match (touches, id) {
            (false, _) => None,
            (true, EMPTY) => Some((Some(EMPTY), edge)),
            (true, id) => Some((Some(id), stack)),
        }
    }

    /// The side of the display the empty space `r` lies on. If the dragged
    /// window was the only one docked, the layout empties when it's lifted
    /// out and it docks afresh against this side.
    fn empty_side(&self, area: Rect, r: Rect) -> Edge {
        let dx = (r.x + r.w / 2 - (area.x + area.w / 2)) as f32 / area.w as f32;
        let dy = (r.y + r.h / 2 - (area.y + area.h / 2)) as f32 / area.h as f32;
        match (dx.abs() >= dy.abs(), dx < 0.0, dy < 0.0) {
            (true, true, _) => Edge::Left,
            (true, false, _) => Edge::Right,
            (false, _, true) => Edge::Top,
            (false, _, false) => Edge::Bottom,
        }
    }

    /// Where `id` would land if docked at `edge` of `target` (see `dock`),
    /// for showing the drop zone before the user lets go.
    pub fn preview(&self, id: WinId, edge: Edge, target: Option<WinId>, area: Rect) -> Option<Rect> {
        let mut l = self.clone();
        l.remove(id);
        l.dock(id, edge, target);
        l.layout(area).into_iter().find(|(w, _)| *w == id).map(|(_, r)| r)
    }
}

/// Which side of the display a drop at (px, py) docks against, if any.
///
/// Only the pointer counts, not how far the window hangs off: wide windows
/// overhang a side after almost any move, so going by that made them
/// impossible to float. The drop-zone highlight shows when the pointer is
/// close enough.
fn screen_edge(area: Rect, px: i32, py: i32) -> Option<Edge> {
    let band = 48;
    if px <= area.x + band {
        return Some(Edge::Left);
    }
    if px >= area.x + area.w - band {
        return Some(Edge::Right);
    }
    // Top only when the pointer is pressed against it: docked windows'
    // title bars live up there, and sliding one sideways shouldn't re-dock.
    if py <= area.y + 2 {
        return Some(Edge::Top);
    }
    if py >= area.y + area.h - band {
        return Some(Edge::Bottom);
    }
    None
}

/// How far in from a window's edge a drop still docks against that edge.
/// Further in is the middle, where a drop leaves the window floating.
fn edge_reach(len: i32) -> i32 {
    (len as f32 * 0.2).min(160.0) as i32
}

/// Manhattan distance from a point to a rect, 0 inside it.
fn dist_to_rect(r: Rect, x: i32, y: i32) -> i32 {
    let dx = (r.x - x).max(x - (r.x + r.w - 1)).max(0);
    let dy = (r.y - y).max(y - (r.y + r.h - 1)).max(0);
    dx + dy
}

fn node_mut<'a>(node: &'a mut Node, path: &[usize]) -> Option<&'a mut Node> {
    match path.split_first() {
        None => Some(node),
        Some((i, rest)) => match node {
            Node::Split { children, .. } => node_mut(children.get_mut(*i)?, rest),
            Node::Leaf(_) => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect::new(0, 0, 1000, 600);

    fn rect_of(l: &Layout, id: WinId) -> Rect {
        l.layout(AREA).into_iter().find(|(w, _)| *w == id).unwrap().1
    }

    #[test]
    fn first_window_takes_its_half() {
        let mut l = Layout::new(0);
        l.dock(1, Edge::Left, None);
        assert_eq!(rect_of(&l, 1), Rect::new(0, 0, 500, 600));
        assert_eq!(l.layout(AREA).len(), 1, "empty space is not a window");
        assert!(!l.contains(EMPTY));
        let mut l = Layout::new(0);
        l.dock(1, Edge::Bottom, None);
        assert_eq!(rect_of(&l, 1), Rect::new(0, 300, 1000, 300));
    }

    #[test]
    fn next_window_fills_the_empty_half() {
        // Docked at the opposite edge.
        let mut l = Layout::new(0);
        l.dock(1, Edge::Left, None);
        l.dock(2, Edge::Right, None);
        assert_eq!(rect_of(&l, 2), Rect::new(500, 0, 500, 600));
        assert!(!l.root.as_ref().unwrap().contains(EMPTY));
        // Dropped into the empty space.
        let mut l = Layout::new(0);
        l.dock(1, Edge::Right, None);
        let target = l.dock_target(AREA, 450, 300, 2);
        assert_eq!(target, Some((Some(EMPTY), Edge::Left)));
        l.dock(2, Edge::Left, Some(EMPTY));
        assert_eq!(rect_of(&l, 2), Rect::new(0, 0, 500, 600));
        assert_eq!(rect_of(&l, 1), Rect::new(500, 0, 500, 600));
    }

    #[test]
    fn lone_window_dragged_into_empty_half_moves_there() {
        let mut l = Layout::new(0);
        l.dock(1, Edge::Left, None);
        let (target, edge) = l.dock_target(AREA, 520, 300, 1).unwrap();
        assert_eq!((target, edge), (Some(EMPTY), Edge::Right));
        // The app undocks the window first, then docks it at the drop.
        l.remove(1);
        l.dock(1, edge, target);
        assert_eq!(rect_of(&l, 1), Rect::new(500, 0, 500, 600));
    }

    #[test]
    fn docking_the_occupied_side_keeps_the_empty_space() {
        let mut l = Layout::new(0);
        l.dock(1, Edge::Left, None);
        l.dock(2, Edge::Left, None);
        assert_eq!(l.layout(AREA).len(), 2);
        assert!(l.root.as_ref().unwrap().contains(EMPTY));
        let r2 = rect_of(&l, 2);
        assert_eq!((r2.x, r2.h), (0, 600));
    }

    #[test]
    fn lone_window_can_be_resized_and_removed() {
        let mut l = Layout::new(0);
        l.dock(1, Edge::Left, None);
        l.apply_resize(1, Rect::new(0, 0, 700, 600), AREA);
        assert_eq!(rect_of(&l, 1), Rect::new(0, 0, 700, 600));
        let p = l.placement_of(1).unwrap();
        l.remove(1);
        assert!(l.is_empty(), "only empty space left means no arrangement");
        l.restore(1, p);
        assert_eq!(rect_of(&l, 1), Rect::new(0, 0, 500, 600));
    }

    #[test]
    fn camera_never_frames_empty_space() {
        let mut l = Layout::new(0);
        l.dock(1, Edge::Left, None);
        l.frame(EMPTY);
        assert!(l.camera.is_empty());
        l.frame(1);
        l.neighbour(1);
        assert!(l.is_visible(1));
    }

    #[test]
    fn middle_of_a_window_or_the_empty_space_floats() {
        let mut l = Layout::new(0);
        l.dock(1, Edge::Left, None);
        assert_eq!(l.dock_target(AREA, 250, 300, 9), None, "middle of window 1");
        assert_eq!(l.dock_target(AREA, 750, 300, 9), None, "middle of the empty half");
    }

    #[test]
    fn side_corners_stack_in_that_column() {
        let mut l = Layout::new(0);
        l.dock(1, Edge::Left, None);
        l.dock(2, Edge::Right, None);
        // Top of the right side: on top of window 2.
        let (t, e) = l.dock_target(AREA, 998, 50, 3).unwrap();
        assert_eq!((t, e), (Some(2), Edge::Top));
        assert_eq!(l.preview(3, e, t, AREA), Some(Rect::new(500, 0, 500, 300)));
        // Bottom of the left side: under window 1.
        assert_eq!(l.dock_target(AREA, 2, 580, 3), Some((Some(1), Edge::Bottom)));
        // Middle of a side still takes the whole side.
        assert_eq!(l.dock_target(AREA, 998, 300, 3), Some((None, Edge::Right)));
    }

    #[test]
    fn top_and_bottom_corners_go_side_by_side() {
        let mut l = Layout::new(0);
        l.dock(1, Edge::Top, None);
        l.dock(2, Edge::Bottom, None);
        let (t, e) = l.dock_target(AREA, 100, 590, 3).unwrap();
        assert_eq!((t, e), (Some(2), Edge::Left));
        assert_eq!(l.preview(3, e, t, AREA), Some(Rect::new(0, 300, 500, 300)));
    }

    #[test]
    fn corner_of_the_empty_side_fills_it() {
        let mut l = Layout::new(0);
        l.dock(1, Edge::Left, None);
        let (t, e) = l.dock_target(AREA, 998, 50, 2).unwrap();
        assert_eq!((t, e), (Some(EMPTY), Edge::Right));
        assert_eq!(l.preview(2, e, t, AREA), Some(Rect::new(500, 0, 500, 600)));
    }

    #[test]
    fn preview_of_the_only_window_moving_sides() {
        let mut l = Layout::new(0);
        l.dock(1, Edge::Left, None);
        assert_eq!(l.preview(1, Edge::Right, None, AREA), Some(Rect::new(500, 0, 500, 600)));
        // Previewing changes nothing.
        assert_eq!(rect_of(&l, 1), Rect::new(0, 0, 500, 600));
    }

    #[test]
    fn top_docks_only_when_pressed_against_it() {
        let l = Layout::new(0);
        assert_eq!(l.dock_target(AREA, 500, 0, 9), Some((None, Edge::Top)));
        assert_eq!(l.dock_target(AREA, 500, 15, 9), None);
    }

    #[test]
    fn dock_left_and_right_share_screen() {
        let mut l = Layout::new(0);
        l.dock(1, Edge::Left, None);
        l.dock(2, Edge::Right, None);
        assert_eq!(rect_of(&l, 1), Rect::new(0, 0, 500, 600));
        assert_eq!(rect_of(&l, 2), Rect::new(500, 0, 500, 600));
    }

    #[test]
    fn same_axis_docks_as_siblings_not_nested() {
        let mut l = Layout::new(0);
        l.dock(1, Edge::Left, None);
        l.dock(2, Edge::Right, None); // fills the empty half
        l.dock(3, Edge::Right, Some(2));
        let Some(Node::Split { children, .. }) = &l.root else { panic!() };
        assert_eq!(children.len(), 3);
    }

    #[test]
    fn nests_on_cross_axis() {
        let mut l = Layout::new(0);
        l.dock(1, Edge::Left, None);
        l.dock(2, Edge::Right, None);
        l.dock(3, Edge::Bottom, Some(2));
        assert_eq!(rect_of(&l, 2), Rect::new(500, 0, 500, 300));
        assert_eq!(rect_of(&l, 3), Rect::new(500, 300, 500, 300));
        assert_eq!(rect_of(&l, 1), Rect::new(0, 0, 500, 600));
    }

    #[test]
    fn remove_collapses_and_reclaims_space() {
        let mut l = Layout::new(0);
        l.dock(1, Edge::Left, None);
        l.dock(2, Edge::Right, None);
        l.dock(3, Edge::Bottom, Some(2));
        l.remove(3);
        assert_eq!(rect_of(&l, 2), Rect::new(500, 0, 500, 600));
        l.remove(2);
        assert_eq!(rect_of(&l, 1), AREA);
        l.remove(1);
        assert!(l.is_empty());
    }

    #[test]
    fn removing_middle_flattens_same_axis() {
        let mut l = Layout::new(0);
        l.dock(1, Edge::Left, None);
        l.dock(2, Edge::Right, None);
        l.dock(3, Edge::Bottom, Some(2));
        l.dock(4, Edge::Right, Some(3)); // 3 and 4 side by side under 2
        l.remove(2); // vertical split collapses to the horizontal [3,4] — merge into root
        let Some(Node::Split { axis, children, .. }) = &l.root else { panic!() };
        assert_eq!(*axis, Axis::Horizontal);
        assert_eq!(children.len(), 3);
    }

    #[test]
    fn zoom_in_fills_screen_and_pushes_others_off() {
        let mut l = Layout::new(0);
        l.dock(1, Edge::Left, None);
        l.dock(2, Edge::Right, None);
        l.frame(2);
        assert_eq!(rect_of(&l, 2), AREA);
        let r1 = rect_of(&l, 1);
        assert!(r1.x + r1.w <= 0, "window 1 should be off-screen left: {r1:?}");
        assert!(l.is_visible(2) && !l.is_visible(1));
        l.zoom_out();
        assert_eq!(rect_of(&l, 2), Rect::new(500, 0, 500, 600));
        l.back();
        assert_eq!(rect_of(&l, 2), AREA);
        l.forward();
        assert!(l.camera.is_empty());
    }

    #[test]
    fn zoom_in_toward_steps_one_level() {
        let mut l = Layout::new(0);
        l.dock(1, Edge::Left, None);
        l.dock(2, Edge::Right, None);
        l.dock(3, Edge::Bottom, Some(2));
        l.zoom_in_toward(3);
        assert_eq!(l.camera, vec![1]);
        assert_eq!(rect_of(&l, 2), Rect::new(0, 0, 1000, 300));
        l.zoom_in_toward(3);
        assert_eq!(rect_of(&l, 3), AREA);
    }

    #[test]
    fn camera_survives_removal() {
        let mut l = Layout::new(0);
        l.dock(1, Edge::Left, None);
        l.dock(2, Edge::Right, None);
        l.frame(2);
        l.remove(2);
        assert!(l.camera.is_empty());
        assert_eq!(rect_of(&l, 1), AREA);
    }

    #[test]
    fn resize_moves_shared_seam() {
        let mut l = Layout::new(0);
        l.dock(1, Edge::Left, None);
        l.dock(2, Edge::Right, None);
        l.apply_resize(1, Rect::new(0, 0, 600, 600), AREA);
        assert_eq!(rect_of(&l, 1), Rect::new(0, 0, 600, 600));
        assert_eq!(rect_of(&l, 2), Rect::new(600, 0, 400, 600));
    }

    #[test]
    fn resize_nested_finds_right_ancestor() {
        let mut l = Layout::new(0);
        l.dock(1, Edge::Left, None);
        l.dock(2, Edge::Right, None);
        l.dock(3, Edge::Bottom, Some(2));
        // Drag window 3's left edge left by 100: the root seam moves.
        l.apply_resize(3, Rect::new(400, 300, 600, 300), AREA);
        assert_eq!(rect_of(&l, 1), Rect::new(0, 0, 400, 600));
        assert_eq!(rect_of(&l, 2), Rect::new(400, 0, 600, 300));
    }

    #[test]
    fn dock_target_edges() {
        let mut l = Layout::new(0);
        l.dock(1, Edge::Left, None);
        l.dock(2, Edge::Right, None);
        assert_eq!(l.dock_target(AREA, 2, 300, 9), Some((None, Edge::Left)));
        assert_eq!(l.dock_target(AREA, 520, 300, 9), Some((Some(2), Edge::Left)));
        assert_eq!(l.dock_target(AREA, 750, 580, 9), Some((None, Edge::Bottom)));
        assert_eq!(l.dock_target(AREA, 750, 300, 9), None);
    }

    #[test]
    fn neighbour_moves_between_siblings() {
        let mut l = Layout::new(0);
        l.dock(1, Edge::Left, None);
        l.dock(2, Edge::Right, None);
        l.frame(1);
        l.neighbour(1);
        assert!(l.is_visible(2) && !l.is_visible(1));
        l.neighbour(1); // no further sibling: stays put
        assert!(l.is_visible(2));
    }

    #[test]
    fn minimize_and_restore_returns_to_same_spot() {
        let mut l = Layout::new(0);
        l.dock(1, Edge::Left, None);
        l.dock(2, Edge::Right, None);
        l.dock(3, Edge::Bottom, Some(2));
        let before = l.layout(AREA);
        for id in [1, 2, 3] {
            let p = l.placement_of(id).unwrap();
            l.remove(id);
            l.restore(id, p);
            let mut a = l.layout(AREA);
            let mut b = before.clone();
            a.sort_by_key(|x| x.0);
            b.sort_by_key(|x| x.0);
            assert_eq!(a, b, "window {id} did not come back to its spot");
        }
    }

    #[test]
    fn ease_endpoints() {
        assert_eq!(ease(0.0), 0.0);
        assert_eq!(ease(1.0), 1.0);
        assert!((ease(0.5) - 0.5).abs() < 1e-6);
    }
}
