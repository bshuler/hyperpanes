//! Mermaid diagrams for the markdown preview.
//!
//! The markdown preview is a flat list of styled rows ([`crate::viewpane`]), which
//! is exactly the wrong shape for a diagram: a graph is two-dimensional and a row
//! model is not. So a ```` ```mermaid ```` fence collapses to *one* row whose
//! payload is a finished [`Diagram`] — node rectangles with absolute positions, and
//! the connecting strokes already flattened into SVG path strings.
//!
//! Everything here is layout, not drawing. `.slint` receives coordinates and paints
//! them; it never sees mermaid source. That is the same rule the rest of the view
//! pane follows, and it is what makes a diagram unit-testable: a test asserts on
//! where a node landed, not on a screenshot.
//!
//! Two dialects are laid out — `flowchart`/`graph` and `sequenceDiagram`, which
//! between them cover almost every diagram that appears in a repository's docs.
//! Anything else is *refused*, not guessed at: [`render`] returns `Err` and the
//! caller falls back to showing the fence as the code it is. A half-drawn class
//! diagram would be worse than an honest code block.

/// A plain rectangle: `A[text]`, and the shape a bare id gets.
pub const SHAPE_RECT: i32 = 0;
/// `A(text)`, `A([text])`, `A[(text)]` — drawn as a rounded/stadium rectangle.
pub const SHAPE_ROUND: i32 = 1;
/// `A{text}`, `A{{text}}` — the only shape that needs a real polygon.
pub const SHAPE_DIAMOND: i32 = 2;
/// `A((text))`.
pub const SHAPE_CIRCLE: i32 = 3;

/// A laid-out node box. Coordinates are top-left, in pixels, inside the diagram's
/// own [`Diagram::w`] x [`Diagram::h`] canvas.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub shape: i32,
    pub text: String,
}

/// A free-standing piece of text: an edge label, or a sequence message. `x`/`y` is
/// the top-left of a centred text box `w` wide.
#[derive(Clone, Debug, PartialEq)]
pub struct Label {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub text: String,
}

/// A finished diagram: boxes, text, and four pre-built path strings.
///
/// The strokes are strings rather than an edge list because Slint's `Path` takes
/// SVG commands directly — one `Path` element draws every solid edge in the
/// diagram, instead of one element per edge. Dashes are segmented here for the
/// same reason: the renderer has no dash-array, so a dotted line ships as many
/// short `M`/`L` pairs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Diagram {
    pub w: f32,
    pub h: f32,
    pub nodes: Vec<Node>,
    pub labels: Vec<Label>,
    /// Solid strokes (`-->`, `---`).
    pub lines: String,
    /// Dotted strokes (`-.->`), already chopped into dashes.
    pub dashed: String,
    /// Thick strokes (`==>`).
    pub thick: String,
    /// Filled arrowheads.
    pub heads: String,
    /// Filled + stroked diamond outlines, which a `Rectangle` cannot express.
    pub diamonds: String,
}

// ---------------------------------------------------------------------------
// limits
// ---------------------------------------------------------------------------

/// Most nodes laid out. Past this the diagram is unreadable at any size, and the
/// caller is better served by the source.
const MAX_NODES: usize = 240;
/// Tallest canvas produced, in px. A pane scrolls, but a 40,000px row does not
/// belong in a preview.
const MAX_CANVAS: f32 = 4_000.0;

const NODE_H: f32 = 34.0;
const DIAMOND_H: f32 = 46.0;
const MIN_NODE_W: f32 = 64.0;
const MAX_NODE_W: f32 = 260.0;
const CHAR_W: f32 = 7.2;
const PAD_X: f32 = 24.0;
/// Gap between two nodes in the same rank.
const GAP_SIBLING: f32 = 28.0;
/// Gap between one rank and the next.
const GAP_RANK: f32 = 52.0;
const MARGIN: f32 = 16.0;

// ---------------------------------------------------------------------------
// entry point
// ---------------------------------------------------------------------------

/// Lay out mermaid `src`, or say why it cannot be.
///
/// The error is a short human sentence, not a code: it is shown to the reader in
/// place of the diagram, so "sequenceDiagram: no messages" beats `E_EMPTY`.
pub fn render(src: &str) -> Result<Diagram, String> {
    let header = src
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("%%"))
        .ok_or_else(|| "empty mermaid block".to_string())?;

    let kind = header.split_whitespace().next().unwrap_or_default();
    match kind {
        "flowchart" | "graph" | "flowchart-v2" => flowchart(src, direction_of(header)),
        "sequenceDiagram" => sequence(src),
        other => Err(format!("{other} diagrams are not rendered yet")),
    }
}

/// Which way a flowchart grows. Mermaid's own default is top-down.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    Down,
    Up,
    Right,
    Left,
}

impl Dir {
    /// Ranks advance along the vertical axis (TD/TB/BT) rather than the horizontal.
    fn vertical(self) -> bool {
        matches!(self, Dir::Down | Dir::Up)
    }
}

fn direction_of(header: &str) -> Dir {
    match header.split_whitespace().nth(1).unwrap_or("TD") {
        "LR" => Dir::Right,
        "RL" => Dir::Left,
        "BT" => Dir::Up,
        // TD, TB, and anything unrecognised: mermaid's default.
        _ => Dir::Down,
    }
}

// ---------------------------------------------------------------------------
// flowchart: parse
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stroke {
    Solid,
    Dotted,
    Thick,
}

#[derive(Debug)]
struct Edge {
    from: usize,
    to: usize,
    stroke: Stroke,
    /// Whether the far end gets an arrowhead (`---` does not).
    arrow: bool,
    label: String,
}

/// A node as parsed, before it has a position.
#[derive(Debug)]
struct Spec {
    id: String,
    text: String,
    shape: i32,
}

#[derive(Default, Debug)]
struct Graph {
    specs: Vec<Spec>,
    edges: Vec<Edge>,
}

impl Graph {
    /// The index of `id`, defining it on first sight. A later mention that carries
    /// a shape upgrades the placeholder — mermaid lets `A --> B` come before
    /// `B[the real label]`, and the label must win wherever it appears.
    fn intern(&mut self, id: &str, text: Option<String>, shape: Option<i32>) -> usize {
        let at = self.specs.iter().position(|s| s.id == id);
        let i = match at {
            Some(i) => i,
            None => {
                self.specs.push(Spec {
                    id: id.to_string(),
                    text: id.to_string(),
                    shape: SHAPE_RECT,
                });
                self.specs.len() - 1
            }
        };
        if let Some(t) = text {
            self.specs[i].text = t;
        }
        if let Some(s) = shape {
            self.specs[i].shape = s;
        }
        i
    }
}

/// Lines that configure rather than describe, and are silently dropped. `subgraph`
/// is here too: the grouping box is not drawn, but its members still are, which
/// reads far better than refusing the whole diagram over it.
fn is_noise(line: &str) -> bool {
    const SKIP: [&str; 9] = [
        "subgraph",
        "end",
        "classDef",
        "class ",
        "style ",
        "click ",
        "linkStyle",
        "direction",
        "accTitle",
    ];
    SKIP.iter().any(|p| line == p.trim_end() || line.starts_with(p))
}

fn flowchart(src: &str, dir: Dir) -> Result<Diagram, String> {
    let mut g = Graph::default();
    for (i, raw) in src.lines().enumerate() {
        let line = strip_comment(raw).trim().trim_end_matches(';').trim();
        // The header line itself carries no statement.
        if i == 0 || line.is_empty() || is_noise(line) {
            continue;
        }
        if line.starts_with("flowchart") || line.starts_with("graph ") {
            continue;
        }
        statement(&mut g, line);
        if g.specs.len() > MAX_NODES {
            return Err(format!("diagram has more than {MAX_NODES} nodes"));
        }
    }
    if g.specs.is_empty() {
        return Err("no nodes in this flowchart".into());
    }
    Ok(place(&g, dir))
}

/// One statement: a chain of nodes joined by edge operators (`A --> B --> C`).
fn statement(g: &mut Graph, line: &str) {
    let chars: Vec<char> = line.chars().collect();
    let mut prev: Option<usize> = None;
    let mut at = 0usize;
    while at < chars.len() {
        let Some((end, op)) = next_edge(&chars, at) else {
            // No operator left. On the first pass that means a bare declaration
            // (`A[Label]`), which still defines a node; later it is the empty
            // remainder of a chain already consumed.
            let tail = chars[at..].iter().collect::<String>();
            node_token(g, &tail);
            return;
        };
        let head = chars[at..end].iter().collect::<String>();
        let from = node_token(g, &head).or(prev);
        let after = op.end;
        // `-->|label|` — the pipe form binds to the operator, not to the target.
        let (label, after) = pipe_label(&chars, after).unwrap_or((op.label.clone(), after));

        // Find where the target token stops: at the next operator, or end of line.
        let stop = next_edge(&chars, after).map(|(s, _)| s).unwrap_or(chars.len());
        let tail = chars[after..stop].iter().collect::<String>();
        let to = node_token(g, &tail);
        if let (Some(a), Some(b)) = (from, to) {
            g.edges.push(Edge {
                from: a,
                to: b,
                stroke: op.stroke,
                arrow: op.arrow,
                label,
            });
        }
        prev = to;
        at = stop;
    }
}

/// A `%%` comment runs to end of line, but only outside a label.
fn strip_comment(line: &str) -> &str {
    match line.find("%%") {
        Some(i) if !line[..i].contains('[') && !line[..i].contains('(') => &line[..i],
        _ => line,
    }
}

struct Op {
    stroke: Stroke,
    arrow: bool,
    label: String,
    /// One past the last char of the operator.
    end: usize,
}

/// The next edge operator at or after `from`, as `(start, op)`.
///
/// An operator must open with `--`, `-.` or `==`, which is what keeps a hyphenated
/// id (`my-node`) from reading as one. Bracket depth is tracked so an arrow inside
/// a label (`A[x --> y]`) is text.
fn next_edge(chars: &[char], from: usize) -> Option<(usize, Op)> {
    let mut depth = 0i32;
    let mut quoted = false;
    let mut i = from;
    while i < chars.len() {
        let c = chars[i];
        if c == '"' {
            quoted = !quoted;
            i += 1;
            continue;
        }
        if !quoted {
            match c {
                '[' | '(' | '{' => depth += 1,
                ']' | ')' | '}' => depth -= 1,
                _ => {}
            }
            if depth <= 0 {
                if let Some(op) = edge_at(chars, i) {
                    return Some((i, op));
                }
            }
        }
        i += 1;
    }
    None
}

/// Try to read an operator starting exactly at `i`.
fn edge_at(chars: &[char], i: usize) -> Option<Op> {
    let two: String = chars.iter().skip(i).take(2).collect();
    let stroke = match two.as_str() {
        "--" => Stroke::Solid,
        "-." => Stroke::Dotted,
        "==" => Stroke::Thick,
        _ => return None,
    };
    let mut j = i;
    while j < chars.len() && matches!(chars[j], '-' | '.' | '=') {
        j += 1;
    }
    // `-->`, `--o`, `--x`: the operator closes here.
    if j < chars.len() && matches!(chars[j], '>' | 'o' | 'x') {
        return Some(Op {
            stroke,
            arrow: true,
            label: String::new(),
            end: j + 1,
        });
    }
    // `A --- B`: an undirected edge, also closed.
    if j > i + 1 && (j >= chars.len() || chars[j] == ' ') {
        // Could still be the opening half of `-- text -->`. Look for a closing run
        // on the same line; if there is one, the middle is the label.
        if let Some((k, close)) = closing_run(chars, j) {
            let label: String = chars[j..k].iter().collect();
            let label = label.trim().to_string();
            // A closing run more than a few words away is a different edge, not a
            // label — `A --- B --- C` must stay two plain edges.
            if !label.is_empty() && !label.contains("--") && label.len() <= 60 {
                return Some(Op {
                    stroke,
                    arrow: close.arrow,
                    label,
                    end: close.end,
                });
            }
        }
        return Some(Op {
            stroke,
            arrow: false,
            label: String::new(),
            end: j,
        });
    }
    None
}

struct Close {
    arrow: bool,
    end: usize,
}

/// The `-->` half of a `-- text -->`, searched for after the opening run.
fn closing_run(chars: &[char], from: usize) -> Option<(usize, Close)> {
    let mut i = from;
    while i < chars.len() {
        if matches!(chars[i], '-' | '.' | '=') {
            let start = i;
            let mut j = i;
            while j < chars.len() && matches!(chars[j], '-' | '.' | '=') {
                j += 1;
            }
            let arrow = j < chars.len() && matches!(chars[j], '>' | 'o' | 'x');
            // A single `-` inside the label (`re-try`) is not a closing run.
            if j - start >= 2 || arrow {
                return Some((
                    start,
                    Close {
                        arrow,
                        end: if arrow { j + 1 } else { j },
                    },
                ));
            }
            i = j;
            continue;
        }
        i += 1;
    }
    None
}

/// `|label|` immediately after an operator.
fn pipe_label(chars: &[char], from: usize) -> Option<(String, usize)> {
    let mut i = from;
    while i < chars.len() && chars[i] == ' ' {
        i += 1;
    }
    if i >= chars.len() || chars[i] != '|' {
        return None;
    }
    let start = i + 1;
    let mut j = start;
    while j < chars.len() && chars[j] != '|' {
        j += 1;
    }
    if j >= chars.len() {
        return None;
    }
    Some((clean_text(&chars[start..j].iter().collect::<String>()), j + 1))
}

/// Read `id`, `id[Text]`, `id(Text)`, `id{Text}`, `id((Text))` … into the graph.
/// `None` for an empty token, so a trailing operator does not mint a blank node.
fn node_token(g: &mut Graph, tok: &str) -> Option<usize> {
    let tok = tok.trim();
    if tok.is_empty() {
        return None;
    }
    // The id runs up to the first opening bracket.
    let open = tok.find(['[', '(', '{', '>']);
    let Some(open) = open else {
        let id = tok.trim();
        // `A & B --> C` — a shared-edge list. Only the first is wired; the form is
        // rare enough that half-support beats a parse failure.
        let id = id.split('&').next().unwrap_or(id).trim();
        if id.is_empty() {
            return None;
        }
        return Some(g.intern(id, None, None));
    };
    let id = tok[..open].trim();
    if id.is_empty() {
        return None;
    }
    let body = &tok[open..];
    let (shape, inner) = shape_of(body);
    Some(g.intern(id, Some(clean_text(inner)), Some(shape)))
}

/// Which bracket pair wraps a node's label, and the label inside it.
fn shape_of(body: &str) -> (i32, &str) {
    const FORMS: [(&str, &str, i32); 9] = [
        ("((", "))", SHAPE_CIRCLE),
        ("([", "])", SHAPE_ROUND),
        ("[(", ")]", SHAPE_ROUND),
        ("[[", "]]", SHAPE_RECT),
        ("{{", "}}", SHAPE_DIAMOND),
        ("[", "]", SHAPE_RECT),
        ("(", ")", SHAPE_ROUND),
        ("{", "}", SHAPE_DIAMOND),
        (">", "]", SHAPE_RECT),
    ];
    for (open, close, shape) in FORMS {
        if let Some(rest) = body.strip_prefix(open) {
            let inner = rest.strip_suffix(close).unwrap_or(rest);
            return (shape, inner);
        }
    }
    (SHAPE_RECT, body)
}

/// A label as it should read on screen: quotes dropped, `<br>` flattened to a
/// space (a node is one line), entities left alone.
fn clean_text(s: &str) -> String {
    let mut t = s.trim().to_string();
    for br in ["<br/>", "<br />", "<br>"] {
        t = t.replace(br, " ");
    }
    let t = t.trim();
    let t = t.strip_prefix('"').unwrap_or(t);
    let t = t.strip_suffix('"').unwrap_or(t);
    t.trim().to_string()
}

// ---------------------------------------------------------------------------
// flowchart: layout
// ---------------------------------------------------------------------------

fn node_w(text: &str, shape: i32) -> f32 {
    let chars = text.chars().count() as f32;
    let w = (chars * CHAR_W + PAD_X).clamp(MIN_NODE_W, MAX_NODE_W);
    if shape == SHAPE_DIAMOND || shape == SHAPE_CIRCLE {
        (w + 20.0).min(MAX_NODE_W)
    } else {
        w
    }
}

fn node_h(shape: i32) -> f32 {
    if shape == SHAPE_DIAMOND {
        DIAMOND_H
    } else {
        NODE_H
    }
}

/// Assign every node a rank, then a position within it.
///
/// Ranking is longest-path relaxation rather than a topological sort, because a
/// mermaid graph in the wild is not reliably acyclic and a sort that assumes it is
/// would either hang or drop the back edge. Relaxation is bounded by the node
/// count, so a cycle costs iterations and nothing worse: the back edge simply
/// stops raising the rank.
fn place(g: &Graph, dir: Dir) -> Diagram {
    let n = g.specs.len();
    let mut rank = vec![0usize; n];
    for _ in 0..n {
        let mut moved = false;
        for e in &g.edges {
            if e.from != e.to && rank[e.to] < rank[e.from] + 1 && rank[e.from] + 1 < n {
                rank[e.to] = rank[e.from] + 1;
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }

    // Rank membership, in first-appearance order — the order the author wrote.
    let depth = rank.iter().copied().max().unwrap_or(0) + 1;
    let mut ranks: Vec<Vec<usize>> = vec![Vec::new(); depth];
    for (i, r) in rank.iter().enumerate() {
        ranks[*r].push(i);
    }

    let mut nodes: Vec<Node> = g
        .specs
        .iter()
        .map(|s| Node {
            x: 0.0,
            y: 0.0,
            w: node_w(&s.text, s.shape),
            h: node_h(s.shape),
            shape: s.shape,
            text: s.text.clone(),
        })
        .collect();

    // Along-rank extent of each rank, and the widest of them: the canvas is sized
    // to the widest rank so every rank can be centred inside it.
    let along = |nd: &Node| if dir.vertical() { nd.w } else { nd.h };
    let across = |nd: &Node| if dir.vertical() { nd.h } else { nd.w };
    let mut extents: Vec<f32> = Vec::with_capacity(depth);
    let mut thick: Vec<f32> = Vec::with_capacity(depth);
    for members in &ranks {
        let sum: f32 = members.iter().map(|i| along(&nodes[*i])).sum();
        let gaps = GAP_SIBLING * (members.len().saturating_sub(1)) as f32;
        extents.push(sum + gaps);
        thick.push(
            members
                .iter()
                .map(|i| across(&nodes[*i]))
                .fold(0.0f32, f32::max),
        );
    }
    let widest = extents.iter().copied().fold(0.0f32, f32::max);

    // Rank offsets down (or across) the canvas.
    let mut rank_at: Vec<f32> = Vec::with_capacity(depth);
    let mut run = MARGIN;
    for t in &thick {
        rank_at.push(run);
        run += t + GAP_RANK;
    }
    let deep = (run - GAP_RANK + MARGIN).min(MAX_CANVAS);
    let wide = widest + MARGIN * 2.0;

    for (r, members) in ranks.iter().enumerate() {
        let mut cursor = MARGIN + (widest - extents[r]) / 2.0;
        for i in members {
            let (a, b) = (along(&nodes[*i]), across(&nodes[*i]));
            let cross = rank_at[r] + (thick[r] - b) / 2.0;
            if dir.vertical() {
                nodes[*i].x = cursor;
                nodes[*i].y = cross;
            } else {
                nodes[*i].y = cursor;
                nodes[*i].x = cross;
            }
            cursor += a + GAP_SIBLING;
        }
    }

    let (w, h) = if dir.vertical() {
        (wide, deep)
    } else {
        (deep, wide)
    };
    // BT and RL are TD and LR reflected: laying them out twice would be two code
    // paths to keep in agreement, and one mirror is exact.
    if dir == Dir::Up {
        for nd in &mut nodes {
            nd.y = h - nd.y - nd.h;
        }
    } else if dir == Dir::Left {
        for nd in &mut nodes {
            nd.x = w - nd.x - nd.w;
        }
    }

    let mut d = Diagram {
        w,
        h,
        nodes,
        ..Default::default()
    };
    for e in &g.edges {
        connect(&mut d, e);
    }
    for nd in &d.nodes {
        if nd.shape == SHAPE_DIAMOND {
            d.diamonds.push_str(&diamond_path(nd));
        }
    }
    d
}

/// Draw one edge: a straight run between the two node borders, an arrowhead if the
/// operator had one, and the label parked at the midpoint.
fn connect(d: &mut Diagram, e: &Edge) {
    let (a, b) = (&d.nodes[e.from], &d.nodes[e.to]);
    let (acx, acy) = (a.x + a.w / 2.0, a.y + a.h / 2.0);
    let (bcx, bcy) = (b.x + b.w / 2.0, b.y + b.h / 2.0);
    let (dx, dy) = (bcx - acx, bcy - acy);
    if dx == 0.0 && dy == 0.0 {
        return;
    }
    let (sx, sy) = border(acx, acy, a.w, a.h, dx, dy);
    let (ex, ey) = border(bcx, bcy, b.w, b.h, -dx, -dy);

    let seg = match e.stroke {
        Stroke::Dotted => dashes(sx, sy, ex, ey),
        _ => format!("M {} {} L {} {} ", n1(sx), n1(sy), n1(ex), n1(ey)),
    };
    match e.stroke {
        Stroke::Solid => d.lines.push_str(&seg),
        Stroke::Dotted => d.dashed.push_str(&seg),
        Stroke::Thick => d.thick.push_str(&seg),
    }
    if e.arrow {
        d.heads.push_str(&arrowhead(sx, sy, ex, ey));
    }
    if !e.label.is_empty() {
        let w = e.label.chars().count() as f32 * 6.4 + 10.0;
        d.labels.push(Label {
            x: (sx + ex) / 2.0 - w / 2.0,
            y: (sy + ey) / 2.0 - 8.0,
            w,
            text: e.label.clone(),
        });
    }
}

/// Where the ray leaving `(cx, cy)` in direction `(dx, dy)` crosses the node's
/// box, pushed 2px clear so the stroke does not touch the border.
fn border(cx: f32, cy: f32, w: f32, h: f32, dx: f32, dy: f32) -> (f32, f32) {
    let (hw, hh) = (w / 2.0, h / 2.0);
    let tx = if dx.abs() > 0.001 {
        hw / dx.abs()
    } else {
        f32::MAX
    };
    let ty = if dy.abs() > 0.001 {
        hh / dy.abs()
    } else {
        f32::MAX
    };
    let t = tx.min(ty);
    let len = (dx * dx + dy * dy).sqrt();
    let pad = if len > 0.0 { 2.0 / len } else { 0.0 };
    (cx + dx * (t + pad), cy + dy * (t + pad))
}

/// A dotted line, chopped by hand: the renderer has no dash-array, so the dashes
/// are geometry like everything else here.
fn dashes(x1: f32, y1: f32, x2: f32, y2: f32) -> String {
    const ON: f32 = 5.0;
    const OFF: f32 = 4.0;
    let (dx, dy) = (x2 - x1, y2 - y1);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1.0 {
        return String::new();
    }
    let (ux, uy) = (dx / len, dy / len);
    let mut out = String::new();
    let mut at = 0.0f32;
    while at < len {
        let end = (at + ON).min(len);
        out.push_str(&format!(
            "M {} {} L {} {} ",
            n1(x1 + ux * at),
            n1(y1 + uy * at),
            n1(x1 + ux * end),
            n1(y1 + uy * end)
        ));
        at = end + OFF;
    }
    out
}

/// A filled triangle at `(x2, y2)`, pointing the way the line runs.
fn arrowhead(x1: f32, y1: f32, x2: f32, y2: f32) -> String {
    const LEN: f32 = 9.0;
    const HALF: f32 = 4.5;
    let (dx, dy) = (x2 - x1, y2 - y1);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 0.001 {
        return String::new();
    }
    let (ux, uy) = (dx / len, dy / len);
    // The normal, for the two back corners.
    let (nx, ny) = (-uy, ux);
    let (bx, by) = (x2 - ux * LEN, y2 - uy * LEN);
    format!(
        "M {} {} L {} {} L {} {} Z ",
        n1(x2),
        n1(y2),
        n1(bx + nx * HALF),
        n1(by + ny * HALF),
        n1(bx - nx * HALF),
        n1(by - ny * HALF)
    )
}

fn diamond_path(nd: &Node) -> String {
    let (cx, cy) = (nd.x + nd.w / 2.0, nd.y + nd.h / 2.0);
    format!(
        "M {} {} L {} {} L {} {} L {} {} Z ",
        n1(nd.x),
        n1(cy),
        n1(cx),
        n1(nd.y),
        n1(nd.x + nd.w),
        n1(cy),
        n1(cx),
        n1(nd.y + nd.h)
    )
}

/// One decimal is the whole precision a px-space path needs, and it keeps the
/// command strings short enough to be worth diffing in a test.
fn n1(v: f32) -> String {
    format!("{v:.1}")
}

// ---------------------------------------------------------------------------
// sequence diagrams
// ---------------------------------------------------------------------------

const SEQ_HEAD_H: f32 = 34.0;
const SEQ_ROW_H: f32 = 38.0;
const SEQ_GAP: f32 = 56.0;
const SEQ_SELF_W: f32 = 44.0;

struct Msg {
    from: usize,
    to: usize,
    dotted: bool,
    arrow: bool,
    text: String,
}

/// Participants across the top, lifelines down, one row per message.
///
/// The same [`Diagram`] carries it: a lifeline is a dashed path, a message is a
/// solid one with an arrowhead, and a participant is a node box. Nothing about the
/// painting side has to know which dialect it is drawing.
fn sequence(src: &str) -> Result<Diagram, String> {
    let mut names: Vec<(String, String)> = Vec::new(); // (id, label)
    let mut msgs: Vec<Msg> = Vec::new();
    let mut intern = |names: &mut Vec<(String, String)>, id: &str| -> usize {
        let id = id.trim();
        if let Some(i) = names.iter().position(|(k, _)| k == id) {
            return i;
        }
        names.push((id.to_string(), id.to_string()));
        names.len() - 1
    };

    for (i, raw) in src.lines().enumerate() {
        let line = strip_comment(raw).trim();
        if i == 0 || line.is_empty() || line.starts_with("%%") {
            continue;
        }
        if let Some(rest) = line
            .strip_prefix("participant ")
            .or_else(|| line.strip_prefix("actor "))
        {
            let (id, label) = match rest.split_once(" as ") {
                Some((a, b)) => (a.trim(), clean_text(b)),
                None => (rest.trim(), clean_text(rest)),
            };
            let at = intern(&mut names, id);
            names[at].1 = label;
            continue;
        }
        // Everything structural is skipped rather than refused: an `alt`/`loop`
        // block still has readable messages inside it.
        const SKIP: [&str; 12] = [
            "alt ", "else", "opt ", "loop ", "par ", "and ", "end", "note ", "Note ", "rect ",
            "activate ", "deactivate ",
        ];
        if SKIP.iter().any(|p| line.starts_with(p)) {
            continue;
        }
        if let Some(m) = seq_message(line, &mut names, &mut intern) {
            msgs.push(m);
        }
        if names.len() > MAX_NODES {
            return Err(format!("diagram has more than {MAX_NODES} participants"));
        }
    }
    if names.is_empty() {
        return Err("no participants in this sequence diagram".into());
    }

    // Columns: each participant is a box, spaced by the wider of its own label and
    // the fixed gap, so a long name never overlaps its neighbour.
    let mut nodes: Vec<Node> = Vec::with_capacity(names.len());
    let mut x = MARGIN;
    for (_, label) in &names {
        let w = node_w(label, SHAPE_RECT);
        nodes.push(Node {
            x,
            y: MARGIN,
            w,
            h: SEQ_HEAD_H,
            shape: SHAPE_ROUND,
            text: label.clone(),
        });
        x += w + SEQ_GAP;
    }
    let w = x - SEQ_GAP + MARGIN;
    let body_top = MARGIN + SEQ_HEAD_H;
    let h = (body_top + SEQ_ROW_H * (msgs.len() as f32 + 0.6) + MARGIN).min(MAX_CANVAS);

    let mut d = Diagram {
        w,
        h,
        nodes,
        ..Default::default()
    };
    for nd in &d.nodes {
        let cx = nd.x + nd.w / 2.0;
        d.dashed.push_str(&dashes(cx, body_top + 4.0, cx, h - MARGIN));
    }
    for (i, m) in msgs.iter().enumerate() {
        let y = body_top + SEQ_ROW_H * (i as f32 + 0.8);
        let ax = d.nodes[m.from].x + d.nodes[m.from].w / 2.0;
        let bx = d.nodes[m.to].x + d.nodes[m.to].w / 2.0;
        let (sx, ex, ly, lw) = if m.from == m.to {
            // A self-call loops out to the right and back.
            let out = ax + SEQ_SELF_W;
            let leg = format!(
                "M {} {} L {} {} L {} {} L {} {} ",
                n1(ax),
                n1(y - 8.0),
                n1(out),
                n1(y - 8.0),
                n1(out),
                n1(y + 6.0),
                n1(ax + 4.0),
                n1(y + 6.0)
            );
            if m.dotted {
                d.dashed.push_str(&leg);
            } else {
                d.lines.push_str(&leg);
            }
            if m.arrow {
                d.heads.push_str(&arrowhead(out, y + 6.0, ax + 4.0, y + 6.0));
            }
            let lw = m.text.chars().count() as f32 * 6.4 + 10.0;
            (ax, out, y - 24.0, lw)
        } else {
            let dir = if bx > ax { 1.0 } else { -1.0 };
            let sx = ax + dir * 4.0;
            let ex = bx - dir * 4.0;
            let seg = if m.dotted {
                dashes(sx, y, ex, y)
            } else {
                format!("M {} {} L {} {} ", n1(sx), n1(y), n1(ex), n1(y))
            };
            if m.dotted {
                d.dashed.push_str(&seg);
            } else {
                d.lines.push_str(&seg);
            }
            if m.arrow {
                d.heads.push_str(&arrowhead(sx, y, ex, y));
            }
            let lw = (ex - sx).abs().max(60.0);
            ((sx.min(ex)), sx.max(ex), y - 18.0, lw)
        };
        if !m.text.is_empty() {
            d.labels.push(Label {
                x: (sx + ex) / 2.0 - lw / 2.0,
                y: ly,
                w: lw,
                text: m.text.clone(),
            });
        }
    }
    Ok(d)
}

/// `A->>B: text` and its relatives. The arrow forms differ only in dashing and
/// whether the head is drawn, which is all the geometry needs to know.
fn seq_message(
    line: &str,
    names: &mut Vec<(String, String)>,
    intern: &mut impl FnMut(&mut Vec<(String, String)>, &str) -> usize,
) -> Option<Msg> {
    // Longest first: `-->>` must not match as `-->`.
    const FORMS: [(&str, bool, bool); 8] = [
        ("-->>", true, true),
        ("--)", true, true),
        ("-->", true, false),
        ("--x", true, true),
        ("->>", false, true),
        ("-)", false, true),
        ("->", false, false),
        ("-x", false, true),
    ];
    let (body, text) = match line.split_once(':') {
        Some((a, b)) => (a, clean_text(b)),
        None => (line, String::new()),
    };
    for (op, dotted, arrow) in FORMS {
        if let Some((a, b)) = body.split_once(op) {
            if a.trim().is_empty() || b.trim().is_empty() {
                continue;
            }
            let from = intern(names, a.trim());
            let to = intern(names, b.trim());
            return Some(Msg {
                from,
                to,
                dotted,
                arrow,
                text,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flow(src: &str) -> Diagram {
        render(src).expect("should lay out")
    }

    fn find<'a>(d: &'a Diagram, text: &str) -> &'a Node {
        d.nodes
            .iter()
            .find(|n| n.text == text)
            .unwrap_or_else(|| panic!("no node {text:?} in {:?}", d.nodes))
    }

    #[test]
    fn a_dialect_we_do_not_draw_is_refused_rather_than_guessed_at() {
        assert!(render("classDiagram\n  A <|-- B").is_err());
        assert!(render("gantt\n  title x").is_err());
        assert!(render("   \n\n").is_err());
    }

    #[test]
    fn every_bracket_form_picks_its_own_shape() {
        let d = flow("flowchart TD\n A[r] --> B(round)\n B --> C{d}\n C --> D((c))\n C --> E([s])");
        assert_eq!(find(&d, "r").shape, SHAPE_RECT);
        assert_eq!(find(&d, "round").shape, SHAPE_ROUND);
        assert_eq!(find(&d, "d").shape, SHAPE_DIAMOND);
        assert_eq!(find(&d, "c").shape, SHAPE_CIRCLE);
        assert_eq!(find(&d, "s").shape, SHAPE_ROUND);
        // Only the diamond needs a polygon; everything else is a rectangle.
        assert!(!d.diamonds.is_empty());
        assert_eq!(d.diamonds.matches('Z').count(), 1);
    }

    #[test]
    fn a_chain_becomes_one_edge_per_link() {
        let d = flow("graph LR\n A --> B --> C --> D");
        assert_eq!(d.nodes.len(), 4);
        // Three arrowheads, one per link.
        assert_eq!(d.heads.matches('Z').count(), 3);
    }

    #[test]
    fn a_label_reads_the_same_written_either_way() {
        let piped = flow("flowchart TD\n A -->|yes| B");
        let inline = flow("flowchart TD\n A -- yes --> B");
        assert_eq!(piped.labels.len(), 1);
        assert_eq!(piped.labels[0].text, "yes");
        assert_eq!(inline.labels.len(), 1);
        assert_eq!(inline.labels[0].text, "yes");
    }

    #[test]
    fn a_hyphenated_id_is_not_read_as_an_arrow() {
        let d = flow("flowchart TD\n my-node --> other-node");
        assert_eq!(d.nodes.len(), 2, "{:?}", d.nodes);
        assert!(d.nodes.iter().any(|n| n.text == "my-node"));
    }

    #[test]
    fn an_arrow_inside_a_label_is_text() {
        let d = flow("flowchart TD\n A[a --> b] --> B");
        assert_eq!(d.nodes.len(), 2, "{:?}", d.nodes);
        assert_eq!(find(&d, "a --> b").shape, SHAPE_RECT);
    }

    #[test]
    fn the_stroke_style_decides_which_path_the_edge_joins() {
        let d = flow("flowchart TD\n A --> B\n B -.-> C\n C ==> D\n D --- E");
        assert!(!d.lines.is_empty());
        assert!(!d.dashed.is_empty(), "a dotted edge must be segmented");
        assert!(!d.thick.is_empty());
        // Three arrows; `---` carries none.
        assert_eq!(d.heads.matches('Z').count(), 3);
    }

    #[test]
    fn a_dotted_edge_is_chopped_into_more_than_one_dash() {
        let d = flow("flowchart LR\n Alpha -.-> Beta");
        assert!(
            d.dashed.matches('M').count() > 1,
            "no dash-array in the renderer, so the dashes must be geometry: {:?}",
            d.dashed
        );
    }

    #[test]
    fn rank_is_the_longest_path_so_a_join_waits_for_its_deepest_input() {
        let d = flow("flowchart TD\n A --> B\n A --> C\n B --> D\n C --> D\n B --> C");
        // C sits below B (A -> B -> C), and D below C — not level with B.
        assert!(find(&d, "C").y > find(&d, "B").y);
        assert!(find(&d, "D").y > find(&d, "C").y);
    }

    #[test]
    fn a_cycle_terminates_instead_of_ranking_forever() {
        let d = flow("flowchart TD\n A --> B\n B --> C\n C --> A");
        assert_eq!(d.nodes.len(), 3);
        assert!(d.h < MAX_CANVAS);
    }

    #[test]
    fn direction_swaps_which_axis_the_ranks_advance_along() {
        let down = flow("flowchart TD\n A --> B");
        let right = flow("flowchart LR\n A --> B");
        assert!(find(&down, "B").y > find(&down, "A").y);
        assert_eq!(find(&down, "A").x.round(), find(&down, "B").x.round());
        assert!(find(&right, "B").x > find(&right, "A").x);
        assert_eq!(find(&right, "A").y.round(), find(&right, "B").y.round());
    }

    #[test]
    fn bt_and_rl_are_the_same_layout_reflected() {
        let up = flow("flowchart BT\n A --> B");
        let left = flow("flowchart RL\n A --> B");
        assert!(find(&up, "B").y < find(&up, "A").y);
        assert!(find(&left, "B").x < find(&left, "A").x);
    }

    #[test]
    fn every_node_lands_inside_the_canvas_it_reports() {
        let d = flow(
            "flowchart TD\n Start([go]) --> Check{ok?}\n Check -->|yes| Done[finish]\n \
             Check -->|no| Retry(try again)\n Retry --> Check",
        );
        for n in &d.nodes {
            assert!(n.x >= 0.0 && n.y >= 0.0, "{n:?}");
            assert!(n.x + n.w <= d.w + 0.5, "{n:?} overflows w={}", d.w);
            assert!(n.y + n.h <= d.h + 0.5, "{n:?} overflows h={}", d.h);
        }
    }

    #[test]
    fn siblings_in_a_rank_do_not_overlap() {
        let d = flow("flowchart TD\n A --> B\n A --> C\n A --> D");
        let mut row: Vec<&Node> = d.nodes.iter().filter(|n| n.text != "A").collect();
        row.sort_by(|a, b| a.x.total_cmp(&b.x));
        for pair in row.windows(2) {
            assert!(pair[0].x + pair[0].w <= pair[1].x, "{pair:?}");
        }
    }

    #[test]
    fn a_subgraph_keeps_its_members_instead_of_failing_the_diagram() {
        let d = flow("flowchart TD\n subgraph one\n A --> B\n end\n B --> C");
        assert_eq!(d.nodes.len(), 3, "{:?}", d.nodes);
    }

    #[test]
    fn styling_lines_are_dropped_not_drawn() {
        let d = flow(
            "flowchart TD\n A --> B\n classDef big fill:#f00\n class A big\n \
             style B stroke:#0f0\n linkStyle 0 stroke:#00f",
        );
        assert_eq!(d.nodes.len(), 2, "{:?}", d.nodes);
    }

    #[test]
    fn a_label_keeps_its_words_but_loses_its_markup() {
        let d = flow("flowchart TD\n A[\"a <br/> b\"] --> B");
        assert_eq!(find(&d, "a   b").text, "a   b");
    }

    #[test]
    fn a_declaration_without_an_edge_still_makes_a_node() {
        let d = flow("flowchart TD\n Lonely[all alone]");
        assert_eq!(d.nodes.len(), 1);
        assert_eq!(d.nodes[0].text, "all alone");
    }

    #[test]
    fn a_late_label_wins_over_the_placeholder_its_edge_made() {
        let d = flow("flowchart TD\n A --> B\n B[the real one]");
        assert_eq!(find(&d, "the real one").shape, SHAPE_RECT);
        assert_eq!(d.nodes.len(), 2);
    }

    #[test]
    fn a_sequence_diagram_puts_its_participants_in_a_row() {
        let d = render("sequenceDiagram\n participant A as Alice\n participant B as Bob\n A->>B: hi\n B-->>A: hello")
            .expect("should lay out");
        assert_eq!(d.nodes.len(), 2);
        assert_eq!(d.nodes[0].text, "Alice");
        assert_eq!(d.nodes[1].text, "Bob");
        // Side by side, on one line.
        assert_eq!(d.nodes[0].y, d.nodes[1].y);
        assert!(d.nodes[1].x > d.nodes[0].x + d.nodes[0].w);
        // Two lifelines plus the dotted reply all live in the dashed path.
        assert!(!d.dashed.is_empty());
        assert_eq!(d.labels.len(), 2);
        assert_eq!(d.heads.matches('Z').count(), 2);
    }

    #[test]
    fn an_undeclared_participant_is_created_by_its_first_message() {
        let d = render("sequenceDiagram\n Web->>Api: GET /x\n Api->>Db: select").unwrap();
        assert_eq!(d.nodes.len(), 3);
        assert_eq!(d.nodes[2].text, "Db");
    }

    #[test]
    fn a_self_message_loops_instead_of_drawing_a_zero_length_line() {
        let d = render("sequenceDiagram\n A->>A: think").unwrap();
        assert_eq!(d.nodes.len(), 1);
        assert!(!d.lines.is_empty());
        assert_eq!(d.heads.matches('Z').count(), 1);
    }

    #[test]
    fn block_keywords_are_skipped_but_the_messages_inside_them_survive() {
        let d = render(
            "sequenceDiagram\n participant A\n participant B\n loop every day\n A->>B: ping\n end\n \
             Note right of B: a note\n B-->>A: pong",
        )
        .unwrap();
        assert_eq!(d.nodes.len(), 2, "{:?}", d.nodes);
        assert_eq!(d.labels.len(), 2, "{:?}", d.labels);
    }

    #[test]
    fn an_enormous_graph_is_refused_before_it_is_laid_out() {
        let mut src = String::from("flowchart TD\n");
        for i in 0..(MAX_NODES + 20) {
            src.push_str(&format!(" n{i} --> n{}\n", i + 1));
        }
        assert!(render(&src).is_err());
    }
}

