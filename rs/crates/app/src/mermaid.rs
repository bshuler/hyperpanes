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
//! Six dialects are laid out: `flowchart`/`graph`, `sequenceDiagram`,
//! `classDiagram`, `stateDiagram`/`stateDiagram-v2`, `erDiagram` and `pie`, which
//! between them cover almost every diagram that appears in a repository's docs.
//! Anything else — `gantt`, `journey`, `mindmap`, `gitGraph`, `quadrantChart` and
//! the rest — is *refused*, not guessed at: [`render`] returns `Err` and the
//! caller falls back to showing the fence as the code it is. A half-drawn gantt
//! chart would be worse than an honest code block, and that stays true for every
//! dialect nobody has sat down and expressed in the primitives below.
//!
//! Those primitives are the whole vocabulary: four node shapes, free-standing
//! labels, and four SVG path strings (`lines`, `dashed`, `thick`, `heads`,
//! `diamonds`). A new dialect earns its place by fitting into them — `heads` is
//! the filled fill and `diamonds` is the outlined one, which is exactly why UML's
//! hollow markers work here and a multi-coloured pie does not.

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
        "classDiagram" | "classDiagram-v2" => class_diagram(src, kind),
        "stateDiagram" | "stateDiagram-v2" => state_diagram(src, kind),
        "erDiagram" => er_diagram(src, kind),
        "pie" => pie_chart(src),
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
    SKIP.iter()
        .any(|p| line == p.trim_end() || line.starts_with(p))
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
        let stop = next_edge(&chars, after)
            .map(|(s, _)| s)
            .unwrap_or(chars.len());
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
    Some((
        clean_text(&chars[start..j].iter().collect::<String>()),
        j + 1,
    ))
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

/// Which edges point back at a node still open on the walk from a root.
///
/// A hand-written mermaid graph is not reliably acyclic, and longest-path
/// relaxation over a cycle does not merely cost iterations — the back edge keeps
/// pushing its own target down, so `A --> B --> C --> A` ranks A *below* C and the
/// diagram reads upside down. Mermaid draws the loop as an edge, not as a rank, so
/// the back edges come out of the ranking and stay in the drawing. Depth-first
/// from each node in author order, which makes the choice of which edge to call
/// the back one deterministic.
fn back_edges(n: usize, edges: &[(usize, usize)]) -> Vec<bool> {
    let mut out: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, (a, b)) in edges.iter().enumerate() {
        if *a < n && *b < n {
            out[*a].push(i);
        }
    }
    // 0 unvisited, 1 open on the current path, 2 finished.
    let mut mark = vec![0u8; n];
    let mut back = vec![false; edges.len()];
    let mut stack: Vec<(usize, usize)> = Vec::new();
    for root in 0..n {
        if mark[root] != 0 {
            continue;
        }
        mark[root] = 1;
        stack.push((root, 0));
        while let Some((v, k)) = stack.pop() {
            if k == out[v].len() {
                mark[v] = 2;
                continue;
            }
            stack.push((v, k + 1));
            let e = out[v][k];
            let w = edges[e].1;
            match mark[w] {
                1 => back[e] = true,
                0 => {
                    mark[w] = 1;
                    stack.push((w, 0));
                }
                _ => {}
            }
        }
    }
    back
}

/// Assign every node a rank, then a position within it.
///
/// Ranking is longest-path relaxation over the graph minus its back edges: a
/// topological sort would hang on a cycle, and relaxing over one inverts it. The
/// node-count bound stays as a backstop.
fn place(g: &Graph, dir: Dir) -> Diagram {
    let n = g.specs.len();
    let pairs: Vec<(usize, usize)> = g.edges.iter().map(|e| (e.from, e.to)).collect();
    let back = back_edges(n, &pairs);
    let mut rank = vec![0usize; n];
    for _ in 0..n {
        let mut moved = false;
        for (i, e) in g.edges.iter().enumerate() {
            if back[i] || e.from == e.to {
                continue;
            }
            if rank[e.to] < rank[e.from] + 1 && rank[e.from] + 1 < n {
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

/// A filled triangle at `(x2, y2)`, pointing the way the line runs. The one
/// arrowhead size the flow and sequence dialects use; [`triangle`] is the same
/// shape with the size left to the caller, for UML's larger open head.
fn arrowhead(x1: f32, y1: f32, x2: f32, y2: f32) -> String {
    triangle(x1, y1, x2, y2, 9.0, 4.5)
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
            "alt ",
            "else",
            "opt ",
            "loop ",
            "par ",
            "and ",
            "end",
            "note ",
            "Note ",
            "rect ",
            "activate ",
            "deactivate ",
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
        d.dashed
            .push_str(&dashes(cx, body_top + 4.0, cx, h - MARGIN));
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
                d.heads
                    .push_str(&arrowhead(out, y + 6.0, ax + 4.0, y + 6.0));
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

// ---------------------------------------------------------------------------
// shared scaffolding for the line-oriented dialects
// ---------------------------------------------------------------------------

/// A block's outer rectangle.
///
/// Three of the dialects below draw a *compartment* — a title box with a body of
/// text rows under it — rather than one node, so ranking and edge routing work on
/// the outer rectangle and the boxes inside it are emitted afterwards. Keeping the
/// two apart is what lets one ranking routine serve class, state and ER diagrams.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl Rect {
    fn cx(self) -> f32 {
        self.x + self.w / 2.0
    }
    fn cy(self) -> f32 {
        self.y + self.h / 2.0
    }
}

/// Height of one text row inside a compartment body.
const ROW_H: f32 = 18.0;
/// Rows kept per compartment. A 60-field entity is a wall of text at preview size,
/// and the reader who needs field 61 wants the source anyway.
const MAX_ROWS: usize = 12;

/// The statement lines of `src`: blanks, `%%` comments and the one header line
/// dropped.
///
/// The flowchart parser does this inline because it also has to watch bracket
/// depth mid-line; everything below is line-oriented and shares this instead.
fn statements(src: &str, header: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut header_seen = false;
    for raw in src.lines() {
        let line = strip_comment(raw).trim().trim_end_matches(';').trim();
        if line.is_empty() {
            continue;
        }
        if !header_seen && line.split_whitespace().next() == Some(header) {
            header_seen = true;
            continue;
        }
        out.push(line.to_string());
    }
    out
}

/// Rank by longest path, the same relaxation [`place`] uses and with the same
/// [`back_edges`] pass in front of it: an inheritance or state chart loops back on
/// itself as readily as a flowchart does.
fn rank_blocks(n: usize, edges: &[(usize, usize)]) -> Vec<usize> {
    let back = back_edges(n, edges);
    let mut rank = vec![0usize; n];
    for _ in 0..n {
        let mut moved = false;
        for (i, &(a, b)) in edges.iter().enumerate() {
            if back[i] || a == b {
                continue;
            }
            if rank[b] < rank[a] + 1 && rank[a] + 1 < n {
                rank[b] = rank[a] + 1;
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }
    rank
}

/// Stack ranked blocks top-down, each rank centred on the widest one.
///
/// Always top-down: `classDiagram` and `erDiagram` have no direction keyword, and
/// a state chart's `direction` is a hint the reader loses nothing by ignoring.
fn stack_blocks(sizes: &[(f32, f32)], rank: &[usize]) -> (Vec<Rect>, f32, f32) {
    let depth = rank.iter().copied().max().unwrap_or(0) + 1;
    let mut rows: Vec<Vec<usize>> = vec![Vec::new(); depth];
    for (i, r) in rank.iter().enumerate() {
        rows[*r].push(i);
    }

    let mut extents: Vec<f32> = Vec::with_capacity(depth);
    let mut tall: Vec<f32> = Vec::with_capacity(depth);
    for members in &rows {
        let sum: f32 = members.iter().map(|i| sizes[*i].0).sum();
        extents.push(sum + GAP_SIBLING * members.len().saturating_sub(1) as f32);
        tall.push(members.iter().map(|i| sizes[*i].1).fold(0.0f32, f32::max));
    }
    let widest = extents.iter().copied().fold(0.0f32, f32::max);

    let mut at: Vec<f32> = Vec::with_capacity(depth);
    let mut run = MARGIN;
    for t in &tall {
        at.push(run);
        run += t + GAP_RANK;
    }
    let h = (run - GAP_RANK + MARGIN).min(MAX_CANVAS);
    let w = widest + MARGIN * 2.0;

    let mut out = vec![Rect::default(); sizes.len()];
    for (r, members) in rows.iter().enumerate() {
        let mut cursor = MARGIN + (widest - extents[r]) / 2.0;
        for i in members {
            out[*i] = Rect {
                x: cursor,
                y: at[r] + (tall[r] - sizes[*i].1) / 2.0,
                w: sizes[*i].0,
                h: sizes[*i].1,
            };
            cursor += sizes[*i].0 + GAP_SIBLING;
        }
    }
    (out, w, h)
}

/// Where a straight run between two blocks meets their borders, or `None` when
/// the two are concentric and there is no direction to leave in.
fn rect_link(a: Rect, b: Rect) -> Option<(f32, f32, f32, f32)> {
    let (dx, dy) = (b.cx() - a.cx(), b.cy() - a.cy());
    if dx == 0.0 && dy == 0.0 {
        return None;
    }
    let (sx, sy) = border(a.cx(), a.cy(), a.w, a.h, dx, dy);
    let (ex, ey) = border(b.cx(), b.cy(), b.w, b.h, -dx, -dy);
    Some((sx, sy, ex, ey))
}

/// How wide a compartment has to be to hold `title` over `rows`.
fn compartment_size(title: &str, rows: &[String]) -> (f32, f32) {
    let mut w = node_w(title, SHAPE_RECT);
    for r in rows {
        w = w.max((r.chars().count() as f32 * 6.4 + 22.0).clamp(MIN_NODE_W, MAX_NODE_W));
    }
    let h = NODE_H
        + if rows.is_empty() {
            0.0
        } else {
            rows.len() as f32 * ROW_H + 6.0
        };
    (w, h)
}

/// Emit a compartment: a title box, a body box under it, and the rows as labels.
///
/// The body is a second *node* rather than a taller title box because the
/// renderer centres a node's text vertically — a title in a 140px-tall box would
/// float in the middle of its own members. Rows are [`Label`]s because labels are
/// painted after nodes, so they land on top of the body instead of behind it, and
/// because a row is text with no border of its own.
fn compartment(d: &mut Diagram, r: Rect, title: &str, rows: &[String]) {
    d.nodes.push(Node {
        x: r.x,
        y: r.y,
        w: r.w,
        h: NODE_H,
        shape: SHAPE_RECT,
        text: title.to_string(),
    });
    if rows.is_empty() {
        return;
    }
    d.nodes.push(Node {
        x: r.x,
        y: r.y + NODE_H,
        w: r.w,
        h: r.h - NODE_H,
        shape: SHAPE_RECT,
        text: String::new(),
    });
    for (i, row) in rows.iter().enumerate() {
        d.labels.push(Label {
            x: r.x + 5.0,
            y: r.y + NODE_H + 4.0 + i as f32 * ROW_H,
            w: r.w - 10.0,
            text: row.clone(),
        });
    }
}

/// Push `row` onto a compartment, replacing the last row with an ellipsis once the
/// box is full — a truncated list that says so beats one that quietly stops.
fn push_row(rows: &mut Vec<String>, row: String) {
    if row.is_empty() {
        return;
    }
    if rows.len() >= MAX_ROWS {
        rows[MAX_ROWS - 1] = "…".to_string();
        return;
    }
    rows.push(row);
}

// ---------------------------------------------------------------------------
// edge end markers
// ---------------------------------------------------------------------------

/// Which shape sits on one end of a relationship, and — as importantly — which of
/// the diagram's two fill styles draws it.
///
/// The renderer offers exactly two: `heads` is filled and unstroked, `diamonds` is
/// surface-filled and stroked. That is the whole reason UML's hollow markers are
/// expressible at all: an "open" triangle or diamond is the *same geometry* routed
/// to the `diamonds` path instead of the `heads` one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tip {
    None,
    /// `-->` association: the ordinary filled arrowhead.
    Arrow,
    /// `<|--` inheritance / `..|>` realization: an open triangle.
    Triangle,
    /// `*--` composition: a filled diamond.
    SolidDiamond,
    /// `o--` aggregation: an open diamond.
    OpenDiamond,
}

/// A triangle whose tip is at `(x2, y2)`, pointing the way the run travels.
fn triangle(x1: f32, y1: f32, x2: f32, y2: f32, len: f32, half: f32) -> String {
    let (dx, dy) = (x2 - x1, y2 - y1);
    let l = (dx * dx + dy * dy).sqrt();
    if l < 0.001 {
        return String::new();
    }
    let (ux, uy) = (dx / l, dy / l);
    let (nx, ny) = (-uy, ux);
    let (bx, by) = (x2 - ux * len, y2 - uy * len);
    format!(
        "M {} {} L {} {} L {} {} Z ",
        n1(x2),
        n1(y2),
        n1(bx + nx * half),
        n1(by + ny * half),
        n1(bx - nx * half),
        n1(by - ny * half)
    )
}

/// A kite whose forward point is at `(x2, y2)` and whose long axis lies along the
/// run — UML's composition/aggregation marker.
fn rhombus(x1: f32, y1: f32, x2: f32, y2: f32, len: f32, half: f32) -> String {
    let (dx, dy) = (x2 - x1, y2 - y1);
    let l = (dx * dx + dy * dy).sqrt();
    if l < 0.001 {
        return String::new();
    }
    let (ux, uy) = (dx / l, dy / l);
    let (nx, ny) = (-uy, ux);
    let (mx, my) = (x2 - ux * len / 2.0, y2 - uy * len / 2.0);
    format!(
        "M {} {} L {} {} L {} {} L {} {} Z ",
        n1(x2),
        n1(y2),
        n1(mx + nx * half),
        n1(my + ny * half),
        n1(x2 - ux * len),
        n1(y2 - uy * len),
        n1(mx - nx * half),
        n1(my - ny * half)
    )
}

/// Draw `tip` at `(x2, y2)`, routing it to whichever of the two fills gives it the
/// right weight. Hollow markers go to `diamonds`, which is stroked over the pane's
/// surface colour and so reads as an outline.
fn marker(d: &mut Diagram, tip: Tip, x1: f32, y1: f32, x2: f32, y2: f32) {
    match tip {
        Tip::None => {}
        Tip::Arrow => d.heads.push_str(&arrowhead(x1, y1, x2, y2)),
        Tip::Triangle => d.diamonds.push_str(&triangle(x1, y1, x2, y2, 12.0, 7.0)),
        Tip::SolidDiamond => d.heads.push_str(&rhombus(x1, y1, x2, y2, 16.0, 6.0)),
        Tip::OpenDiamond => d.diamonds.push_str(&rhombus(x1, y1, x2, y2, 16.0, 6.0)),
    }
}

/// A caption centred on `(cx, cy)`, sized to its own text.
fn caption(d: &mut Diagram, cx: f32, cy: f32, text: &str) {
    if text.is_empty() {
        return;
    }
    let w = text.chars().count() as f32 * 6.4 + 10.0;
    d.labels.push(Label {
        x: cx - w / 2.0,
        y: cy - 8.0,
        w,
        text: text.to_string(),
    });
}

/// The two end captions of a relationship — ER cardinality, or UML multiplicity —
/// parked a fifth of the way in from each end so they clear the node borders and
/// the midpoint label both.
fn end_captions(d: &mut Diagram, run: (f32, f32, f32, f32), at_from: &str, at_to: &str) {
    let (sx, sy, ex, ey) = run;
    caption(d, sx + (ex - sx) * 0.2, sy + (ey - sy) * 0.2 - 2.0, at_from);
    caption(d, sx + (ex - sx) * 0.8, sy + (ey - sy) * 0.8 - 2.0, at_to);
}

/// A filled disc, approximated by a 16-gon.
///
/// A polygon rather than two arcs because `heads` is handed straight to the
/// renderer's SVG path parser and this module has no way to test what that parser
/// accepts; `L` commands are the subset every consumer of the other four strings
/// already proves works.
fn disc(cx: f32, cy: f32, r: f32) -> String {
    const SIDES: usize = 16;
    let mut out = String::new();
    for i in 0..SIDES {
        let a = std::f32::consts::TAU * i as f32 / SIDES as f32;
        let (x, y) = (cx + r * a.cos(), cy + r * a.sin());
        out.push_str(if i == 0 { "M " } else { "L " });
        out.push_str(&format!("{} {} ", n1(x), n1(y)));
    }
    out.push_str("Z ");
    out
}

// ---------------------------------------------------------------------------
// class diagrams
// ---------------------------------------------------------------------------

/// One relationship between two blocks. `from_tip`/`to_tip` are separate because
/// mermaid writes the marker on whichever end owns it (`A <|-- B` decorates `A`),
/// and a few forms decorate both.
struct Rel {
    from: usize,
    to: usize,
    dashed: bool,
    from_tip: Tip,
    to_tip: Tip,
    label: String,
    from_note: String,
    to_note: String,
}

struct Compartment {
    id: String,
    title: String,
    rows: Vec<String>,
}

fn intern_block(boxes: &mut Vec<Compartment>, id: &str) -> usize {
    let id = id.trim();
    if let Some(i) = boxes.iter().position(|b| b.id == id) {
        return i;
    }
    boxes.push(Compartment {
        id: id.to_string(),
        title: id.to_string(),
        rows: Vec::new(),
    });
    boxes.len() - 1
}

/// The relationship operators, longest and most decorated first so `<|--` is never
/// read as the `--` hiding inside it.
const CLASS_REL: [(&str, Tip, Tip, bool); 14] = [
    ("<|..", Tip::Triangle, Tip::None, true),
    ("..|>", Tip::None, Tip::Triangle, true),
    ("<|--", Tip::Triangle, Tip::None, false),
    ("--|>", Tip::None, Tip::Triangle, false),
    ("*--", Tip::SolidDiamond, Tip::None, false),
    ("--*", Tip::None, Tip::SolidDiamond, false),
    ("o--", Tip::OpenDiamond, Tip::None, false),
    ("--o", Tip::None, Tip::OpenDiamond, false),
    ("<..", Tip::Arrow, Tip::None, true),
    ("..>", Tip::None, Tip::Arrow, true),
    ("<--", Tip::Arrow, Tip::None, false),
    ("-->", Tip::None, Tip::Arrow, false),
    ("--", Tip::None, Tip::None, false),
    ("..", Tip::None, Tip::None, true),
];

/// Configuration and prose, dropped the way `is_noise` drops a flowchart's.
fn is_class_noise(line: &str) -> bool {
    const SKIP: [&str; 10] = [
        "direction",
        "namespace",
        "note",
        "click",
        "style",
        "cssClass",
        "callback",
        "link ",
        "accTitle",
        "accDescr",
    ];
    line == "}" || SKIP.iter().any(|p| line.starts_with(p))
}

/// Split `Foo "1"` into the id and the multiplicity written beside it.
fn endpoint(tok: &str) -> (String, String) {
    let mut note = String::new();
    let mut id = String::new();
    let mut quoted = false;
    for c in tok.chars() {
        if c == '"' {
            quoted = !quoted;
            continue;
        }
        if quoted {
            note.push(c);
        } else {
            id.push(c);
        }
    }
    (id.trim().to_string(), note.trim().to_string())
}

/// Boxes with a title and member lines, joined by UML relationships.
///
/// Every marker UML asks for is one of the four [`Tip`]s, which is why this
/// dialect fits the existing primitives at all: the distinction between
/// inheritance and dependency is entirely in the tip and the dashing, and both are
/// things the [`Diagram`] already carries.
fn class_diagram(src: &str, kind: &str) -> Result<Diagram, String> {
    let mut boxes: Vec<Compartment> = Vec::new();
    let mut rels: Vec<Rel> = Vec::new();
    // The class whose `{ … }` body we are inside, if any.
    let mut open: Option<usize> = None;

    for line in statements(src, kind) {
        if let Some(i) = open {
            if line.starts_with('}') {
                open = None;
            } else {
                push_row(&mut boxes[i].rows, clean_text(&line));
            }
            continue;
        }
        if is_class_noise(&line) {
            continue;
        }
        if let Some(head) = line.strip_suffix('{') {
            let name = head.trim().strip_prefix("class ").unwrap_or(head.trim());
            let name = name.trim();
            if !name.is_empty() {
                open = Some(intern_block(&mut boxes, name));
            }
            continue;
        }
        if let Some(name) = line.strip_prefix("class ") {
            intern_block(&mut boxes, name.trim());
            continue;
        }

        // A relationship is decided on the part before the `:`, so a member line
        // (`Foo : +int age`) whose text happens to contain `--` cannot masquerade
        // as one.
        let (body, tail) = match line.split_once(':') {
            Some((a, b)) => (a.trim().to_string(), clean_text(b)),
            None => (line.clone(), String::new()),
        };
        if let Some(r) = class_relation(&mut boxes, &body, &tail) {
            rels.push(r);
        } else if !tail.is_empty() && !body.is_empty() && !body.contains(' ') {
            let i = intern_block(&mut boxes, &body);
            push_row(&mut boxes[i].rows, tail);
        } else if !body.is_empty() && !body.contains(' ') {
            intern_block(&mut boxes, &body);
        }
        if boxes.len() > MAX_NODES {
            return Err(format!("diagram has more than {MAX_NODES} classes"));
        }
    }

    if boxes.is_empty() {
        return Err("no classes in this class diagram".into());
    }
    Ok(blocks_to_diagram(&boxes, &rels))
}

fn class_relation(boxes: &mut Vec<Compartment>, body: &str, label: &str) -> Option<Rel> {
    for (op, from_tip, to_tip, dashed) in CLASS_REL {
        let Some(at) = body.find(op) else { continue };
        let (lhs, rhs) = (&body[..at], &body[at + op.len()..]);
        let (from_id, from_note) = endpoint(lhs);
        let (to_id, to_note) = endpoint(rhs);
        if from_id.is_empty() || to_id.is_empty() {
            continue;
        }
        let from = intern_block(boxes, &from_id);
        let to = intern_block(boxes, &to_id);
        return Some(Rel {
            from,
            to,
            dashed,
            from_tip,
            to_tip,
            label: label.to_string(),
            from_note,
            to_note,
        });
    }
    None
}

/// Rank, place and stroke a set of compartments — the shared back half of the
/// class and ER dialects, which differ only in what they call a row.
fn blocks_to_diagram(boxes: &[Compartment], rels: &[Rel]) -> Diagram {
    let sizes: Vec<(f32, f32)> = boxes
        .iter()
        .map(|b| compartment_size(&b.title, &b.rows))
        .collect();
    let edges: Vec<(usize, usize)> = rels.iter().map(|r| (r.from, r.to)).collect();
    let rank = rank_blocks(boxes.len(), &edges);
    let (rects, w, h) = stack_blocks(&sizes, &rank);

    let mut d = Diagram {
        w,
        h,
        ..Default::default()
    };
    for r in rels {
        let Some(run) = rect_link(rects[r.from], rects[r.to]) else {
            continue;
        };
        let (sx, sy, ex, ey) = run;
        if r.dashed {
            d.dashed.push_str(&dashes(sx, sy, ex, ey));
        } else {
            d.lines
                .push_str(&format!("M {} {} L {} {} ", n1(sx), n1(sy), n1(ex), n1(ey)));
        }
        marker(&mut d, r.to_tip, sx, sy, ex, ey);
        marker(&mut d, r.from_tip, ex, ey, sx, sy);
        caption(&mut d, (sx + ex) / 2.0, (sy + ey) / 2.0, &r.label);
        end_captions(&mut d, run, &r.from_note, &r.to_note);
    }
    for (i, b) in boxes.iter().enumerate() {
        compartment(&mut d, rects[i], &b.title, &b.rows);
    }
    d
}

// ---------------------------------------------------------------------------
// state diagrams
// ---------------------------------------------------------------------------

/// Radius of the `[*]` marker.
const PSEUDO_R: f32 = 9.0;

struct Transition {
    from: usize,
    to: usize,
    label: String,
}

/// The index of state `id` in `(id, label)` order, defining it on first sight.
fn intern_state(names: &mut Vec<(String, String)>, id: &str) -> usize {
    if let Some(i) = names.iter().position(|(k, _)| k == id) {
        return i;
    }
    names.push((id.to_string(), id.to_string()));
    names.len() - 1
}

/// States as rounded boxes, `[*]` as a filled disc, transitions as labelled
/// arrows.
///
/// The two `[*]` pseudo-states are merged into one start and one end rather than
/// minted per mention: mermaid distinguishes them only by which side of the arrow
/// they sit on, and a chart with four exits reads better converging on one dot
/// than sprouting four.
///
/// A pseudo-state gets no [`Node`]. It is a *filled* dot, and a node box paints its
/// own surface colour over anything beneath it — so the disc goes into `heads`,
/// which is the only filled-and-unstroked path the renderer has, and the layout
/// reserves space for it as a block with no box.
fn state_diagram(src: &str, kind: &str) -> Result<Diagram, String> {
    // Ids that no author can collide with, because mermaid ids cannot hold a space.
    const START: &str = "[*] start";
    const END: &str = "[*] end";

    let mut names: Vec<(String, String)> = Vec::new();
    let mut steps: Vec<Transition> = Vec::new();

    for line in statements(src, kind) {
        // `note … : text` and its `end note` block, `direction`, and the `--`
        // concurrency divider carry no state and no transition.
        if line.starts_with("note")
            || line.starts_with("end note")
            || line.starts_with("direction")
            || line.starts_with("class ")
            || line.starts_with("classDef")
            || line == "--"
            || line == "}"
        {
            continue;
        }
        // `state "Long description" as S` and `state S`. The composite form
        // `state S { … }` keeps the state and lets its children fall out flat,
        // which is the same trade `subgraph` makes in a flowchart.
        if let Some(rest) = line.strip_prefix("state ") {
            let rest = rest.trim().trim_end_matches('{').trim();
            // `state Pick <<choice>>` — the stereotype changes how mermaid paints
            // the state, and nothing about where it goes.
            let rest = rest.split("<<").next().unwrap_or(rest).trim();
            let (id, label) = match rest.split_once(" as ") {
                Some((a, b)) => (b.trim().to_string(), clean_text(a)),
                None => (rest.to_string(), clean_text(rest)),
            };
            if id.is_empty() {
                continue;
            }
            let at = intern_state(&mut names, &id);
            names[at].1 = label;
            continue;
        }
        let (body, label) = match line.split_once(':') {
            Some((a, b)) => (a.trim().to_string(), clean_text(b)),
            None => (line.clone(), String::new()),
        };
        let Some((lhs, rhs)) = body.split_once("-->") else {
            continue;
        };
        let (lhs, rhs) = (lhs.trim(), rhs.trim());
        if lhs.is_empty() || rhs.is_empty() {
            continue;
        }
        let from = intern_state(&mut names, if lhs == "[*]" { START } else { lhs });
        let to = intern_state(&mut names, if rhs == "[*]" { END } else { rhs });
        steps.push(Transition { from, to, label });
        if names.len() > MAX_NODES {
            return Err(format!("diagram has more than {MAX_NODES} states"));
        }
    }

    if names.is_empty() {
        return Err("no states in this state diagram".into());
    }

    let pseudo = |id: &str| id == START || id == END;
    let sizes: Vec<(f32, f32)> = names
        .iter()
        .map(|(id, label)| {
            if pseudo(id) {
                (PSEUDO_R * 2.0, PSEUDO_R * 2.0)
            } else {
                (node_w(label, SHAPE_ROUND), NODE_H)
            }
        })
        .collect();
    let edges: Vec<(usize, usize)> = steps.iter().map(|t| (t.from, t.to)).collect();
    let rank = rank_blocks(names.len(), &edges);
    let (rects, w, h) = stack_blocks(&sizes, &rank);

    let mut d = Diagram {
        w,
        h,
        ..Default::default()
    };
    for t in &steps {
        let Some((sx, sy, ex, ey)) = rect_link(rects[t.from], rects[t.to]) else {
            continue;
        };
        d.lines
            .push_str(&format!("M {} {} L {} {} ", n1(sx), n1(sy), n1(ex), n1(ey)));
        d.heads.push_str(&arrowhead(sx, sy, ex, ey));
        caption(&mut d, (sx + ex) / 2.0, (sy + ey) / 2.0, &t.label);
    }
    for (i, (id, label)) in names.iter().enumerate() {
        let r = rects[i];
        if pseudo(id) {
            d.heads.push_str(&disc(r.cx(), r.cy(), PSEUDO_R));
            continue;
        }
        d.nodes.push(Node {
            x: r.x,
            y: r.y,
            w: r.w,
            h: r.h,
            shape: SHAPE_ROUND,
            text: label.clone(),
        });
    }
    Ok(d)
}

// ---------------------------------------------------------------------------
// entity-relationship diagrams
// ---------------------------------------------------------------------------

/// What one end of an ER relationship says about its side's count.
fn er_card(tok: &str) -> Option<&'static str> {
    Some(match tok {
        "||" => "1",
        "|o" | "o|" => "0..1",
        "}|" | "|{" => "1..N",
        "}o" | "o{" => "0..N",
        _ => return None,
    })
}

/// Entity boxes with their attributes, joined by lines carrying the cardinality at
/// each end and the relationship's own verb in the middle.
///
/// ER has no arrowheads at all — a relationship is symmetric — so the whole
/// dialect rides on plain runs plus captions, and the crow's foot is spelled out
/// as `0..N` rather than drawn. Spelling it beats a glyph the primitives cannot
/// make honestly.
fn er_diagram(src: &str, kind: &str) -> Result<Diagram, String> {
    let mut boxes: Vec<Compartment> = Vec::new();
    let mut rels: Vec<Rel> = Vec::new();
    let mut open: Option<usize> = None;

    for line in statements(src, kind) {
        if let Some(i) = open {
            if line.starts_with('}') {
                open = None;
            } else {
                push_row(&mut boxes[i].rows, clean_text(&line));
            }
            continue;
        }
        if line == "}" || line.starts_with("direction") || line.starts_with("acc") {
            continue;
        }
        if let Some(head) = line.strip_suffix('{') {
            let name = head.trim();
            if !name.is_empty() {
                open = Some(intern_block(&mut boxes, name));
            }
            continue;
        }
        let (body, label) = match line.split_once(':') {
            Some((a, b)) => (a.trim().to_string(), clean_text(b)),
            None => (line.clone(), String::new()),
        };
        if let Some(r) = er_relation(&mut boxes, &body, &label) {
            rels.push(r);
        } else if !body.is_empty() && !body.contains(' ') {
            intern_block(&mut boxes, &body);
        }
        if boxes.len() > MAX_NODES {
            return Err(format!("diagram has more than {MAX_NODES} entities"));
        }
    }

    if boxes.is_empty() {
        return Err("no entities in this ER diagram".into());
    }
    Ok(blocks_to_diagram(&boxes, &rels))
}

/// `CUSTOMER ||--o{ ORDER`: a two-character cardinality, a two-character line
/// style, another cardinality. The line style is found first and the cardinalities
/// read off around it, so an entity name containing a single `-` stays a name.
fn er_relation(boxes: &mut Vec<Compartment>, body: &str, label: &str) -> Option<Rel> {
    let chars: Vec<char> = body.chars().collect();
    for i in 2..chars.len().saturating_sub(3) {
        let dashed = match (chars[i], chars[i + 1]) {
            ('-', '-') => false,
            ('.', '.') => true,
            _ => continue,
        };
        let left: String = chars[i - 2..i].iter().collect();
        let right: String = chars[i + 2..i + 4].iter().collect();
        let (Some(lc), Some(rc)) = (er_card(&left), er_card(&right)) else {
            continue;
        };
        let from_id: String = chars[..i - 2].iter().collect();
        let to_id: String = chars[i + 4..].iter().collect();
        let (from_id, to_id) = (from_id.trim(), to_id.trim());
        if from_id.is_empty() || to_id.is_empty() {
            continue;
        }
        let from = intern_block(boxes, from_id);
        let to = intern_block(boxes, to_id);
        return Some(Rel {
            from,
            to,
            dashed,
            from_tip: Tip::None,
            to_tip: Tip::None,
            label: label.to_string(),
            from_note: lc.to_string(),
            to_note: rc.to_string(),
        });
    }
    None
}

// ---------------------------------------------------------------------------
// pie charts
// ---------------------------------------------------------------------------

const PIE_ROW_H: f32 = 26.0;
const PIE_BAR_H: f32 = 16.0;
const PIE_TRACK: f32 = 240.0;
const PIE_PCT_W: f32 = 60.0;
const PIE_NAME_MAX: f32 = 170.0;

/// A `pie` block, laid out as a proportional **bar chart** rather than a circle.
///
/// This is deliberate, and it is a limit of the primitives rather than of effort.
/// A pie is only readable when its wedges are told apart by colour, and this
/// module can hand the renderer exactly two fills: `heads` (one flat colour) and
/// `diamonds` (surface + one stroke). Every wedge would come out the same shade,
/// which is a circle that answers no question the legend below it does not answer
/// better. Wedge geometry would also need arcs, which nothing else here relies on.
/// So each slice becomes a labelled bar whose length is its share — the same
/// information, in a form the four paths can state honestly.
fn pie_chart(src: &str) -> Result<Diagram, String> {
    let mut title = String::new();
    let mut slices: Vec<(String, f32)> = Vec::new();

    for raw in src.lines() {
        let line = strip_comment(raw).trim().to_string();
        if line.is_empty() {
            continue;
        }
        // The header carries the title on its own line: `pie title Pets adopted`.
        if let Some(rest) = line.strip_prefix("pie") {
            let rest = rest.trim();
            if let Some(t) = rest.strip_prefix("title ") {
                title = clean_text(t);
            }
            continue;
        }
        if let Some(t) = line.strip_prefix("title ") {
            title = clean_text(t);
            continue;
        }
        if line.starts_with("acc") {
            continue;
        }
        let Some((name, value)) = line.rsplit_once(':') else {
            continue;
        };
        let Ok(v) = value.trim().parse::<f32>() else {
            continue;
        };
        if v.is_finite() && v > 0.0 {
            slices.push((clean_text(name), v));
        }
        if slices.len() > MAX_NODES {
            return Err(format!("chart has more than {MAX_NODES} slices"));
        }
    }

    if slices.is_empty() {
        return Err("no slices in this pie chart".into());
    }
    let total: f32 = slices.iter().map(|(_, v)| *v).sum();
    let peak = slices.iter().map(|(_, v)| *v).fold(0.0f32, f32::max);

    let name_w = slices
        .iter()
        .map(|(n, _)| n.chars().count() as f32 * 6.4 + 12.0)
        .fold(MIN_NODE_W, f32::max)
        .min(PIE_NAME_MAX);
    let bar_x = MARGIN + name_w + 8.0;
    let pct_x = bar_x + PIE_TRACK + 8.0;
    let top = MARGIN + if title.is_empty() { 0.0 } else { 22.0 };

    let mut d = Diagram {
        w: pct_x + PIE_PCT_W + MARGIN,
        h: (top + slices.len() as f32 * PIE_ROW_H + MARGIN).min(MAX_CANVAS),
        ..Default::default()
    };
    if !title.is_empty() {
        d.labels.push(Label {
            x: MARGIN,
            y: MARGIN,
            w: d.w - MARGIN * 2.0,
            text: title,
        });
    }
    for (i, (name, value)) in slices.iter().enumerate() {
        let y = top + i as f32 * PIE_ROW_H;
        d.labels.push(Label {
            x: MARGIN,
            y: y + (PIE_BAR_H - 15.0) / 2.0 + 2.0,
            w: name_w,
            text: name.clone(),
        });
        // Bars are scaled against the largest slice, not the total, so a chart of
        // three near-equal slices still fills its track and stays comparable.
        d.nodes.push(Node {
            x: bar_x,
            y: y + 2.0,
            w: (PIE_TRACK * value / peak).max(3.0),
            h: PIE_BAR_H,
            shape: SHAPE_RECT,
            text: String::new(),
        });
        d.labels.push(Label {
            x: pct_x,
            y: y + (PIE_BAR_H - 15.0) / 2.0 + 2.0,
            w: PIE_PCT_W,
            text: format!("{:.1}%", value / total * 100.0),
        });
    }
    Ok(d)
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
        // The refusal is the feature: the caller shows the fence as code, which
        // beats a diagram that only looks like the one the author wrote.
        for src in [
            "gantt\n  title x",
            "journey\n  title My day\n  Go to work: 5: Me",
            "mindmap\n  root((idea))",
            "gitGraph\n  commit",
            "quadrantChart\n  title Reach",
            "timeline\n  2024 : shipped",
        ] {
            assert!(render(src).is_err(), "{src:?} must not be guessed at");
        }
        assert!(render("   \n\n").is_err());
    }

    #[test]
    fn a_dialect_we_do_draw_still_refuses_a_body_it_found_nothing_in() {
        // Recognising the header is not the same as having something to draw.
        assert!(render("classDiagram\n  %% nothing yet").is_err());
        assert!(render("stateDiagram-v2\n  direction LR").is_err());
        assert!(render("erDiagram").is_err());
        assert!(render("pie title Empty").is_err());
    }

    // --- class diagrams -----------------------------------------------------

    #[test]
    fn a_class_is_a_title_box_over_a_body_box_holding_its_members() {
        let d =
            flow("classDiagram\n class Animal {\n +String name\n +run()\n }\n Animal <|-- Duck");
        // Animal is two boxes (title + body); Duck, memberless, is one.
        assert_eq!(d.nodes.len(), 3, "{:?}", d.nodes);
        let title = find(&d, "Animal");
        let body = &d.nodes[1];
        assert_eq!(body.text, "");
        assert_eq!(body.y, title.y + title.h, "the body abuts its title");
        assert_eq!(body.w, title.w);
        // Members are labels, so they paint over the body rather than behind it.
        let rows: Vec<&str> = d.labels.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(rows, vec!["+String name", "+run()"]);
        for l in &d.labels {
            assert!(l.y > title.y + title.h, "{l:?} must sit inside the body");
            assert!(l.x >= title.x && l.x + l.w <= title.x + title.w, "{l:?}");
        }
        assert!(d.labels[1].y > d.labels[0].y, "members keep their order");
        // The subclass ranks below the class it extends.
        assert!(find(&d, "Duck").y > title.y);
    }

    #[test]
    fn each_relationship_picks_the_marker_uml_gives_it() {
        let d = flow("classDiagram\n A <|-- B\n C *-- D\n E o-- F\n G --> H\n I ..> J\n K ..|> L");
        // Hollow markers are the ones routed to `diamonds`, which is the only
        // stroked-and-surface-filled path there is: two triangles (<|--, ..|>)
        // plus one open diamond (o--).
        assert_eq!(d.diamonds.matches('Z').count(), 3, "{:?}", d.diamonds);
        // Filled markers go to `heads`: one composition diamond plus the two
        // association/dependency arrows.
        assert_eq!(d.heads.matches('Z').count(), 3, "{:?}", d.heads);
        // `..>` and `..|>` are the dashed pair; the other four are solid.
        assert!(!d.dashed.is_empty());
        assert!(!d.lines.is_empty());
        assert_eq!(d.nodes.len(), 12, "{:?}", d.nodes);
    }

    #[test]
    fn a_member_written_beside_a_colon_joins_the_same_box() {
        let d = flow("classDiagram\n Duck : +String beakColor\n Duck : +swim()");
        assert_eq!(d.nodes.len(), 2, "one title, one body: {:?}", d.nodes);
        assert_eq!(d.labels.len(), 2);
        assert_eq!(d.labels[0].text, "+String beakColor");
    }

    #[test]
    fn a_relationship_keeps_its_verb_in_the_middle_and_its_multiplicity_at_the_ends() {
        let d = flow("classDiagram\n Order \"1\" --> \"*\" Item : contains");
        assert_eq!(
            d.nodes.len(),
            2,
            "the quotes are multiplicity, not ids: {:?}",
            d.nodes
        );
        let texts: Vec<&str> = d.labels.iter().map(|l| l.text.as_str()).collect();
        assert!(texts.contains(&"contains"), "{texts:?}");
        assert!(texts.contains(&"1") && texts.contains(&"*"), "{texts:?}");
        let mid = d.labels.iter().find(|l| l.text == "contains").unwrap();
        let one = d.labels.iter().find(|l| l.text == "1").unwrap();
        // The multiplicity hugs its own end; the verb sits between the two.
        assert!(one.y < mid.y, "{one:?} {mid:?}");
    }

    // --- state diagrams -----------------------------------------------------

    #[test]
    fn a_state_chart_draws_its_pseudo_states_as_filled_dots_and_not_as_boxes() {
        let d = flow("stateDiagram-v2\n [*] --> Still\n Still --> Moving : go\n Moving --> [*]");
        // Only real states get a box. `[*]` is filled, and a node box would paint
        // its own surface colour straight over the dot.
        assert_eq!(d.nodes.len(), 2, "{:?}", d.nodes);
        assert!(d.nodes.iter().all(|n| n.shape == SHAPE_ROUND));
        assert!(find(&d, "Moving").y > find(&d, "Still").y);
        // Three arrowheads plus two discs, all in the one filled path.
        assert_eq!(d.heads.matches('Z').count(), 5, "{:?}", d.heads);
        assert_eq!(d.labels.len(), 1);
        assert_eq!(d.labels[0].text, "go");
        // The start dot ranks above every real state, the end dot below.
        assert!(d.nodes.iter().all(|n| n.y > 0.0));
    }

    #[test]
    fn a_transition_label_lands_between_the_two_states_it_joins() {
        let d = flow("stateDiagram-v2\n Idle --> Busy : work arrives");
        let (a, b) = (find(&d, "Idle"), find(&d, "Busy"));
        let l = &d.labels[0];
        assert_eq!(l.text, "work arrives");
        assert!(l.y > a.y + a.h, "{l:?} is below Idle");
        assert!(l.y < b.y, "{l:?} is above Busy");
    }

    #[test]
    fn a_state_description_replaces_the_id_it_was_declared_against() {
        let d = flow("stateDiagram-v2\n state \"Waiting for input\" as W\n [*] --> W\n W --> [*]");
        assert_eq!(d.nodes.len(), 1, "{:?}", d.nodes);
        assert_eq!(d.nodes[0].text, "Waiting for input");
    }

    #[test]
    fn every_exit_converges_on_one_end_dot_instead_of_sprouting_its_own() {
        let d = flow("stateDiagram\n [*] --> A\n A --> B\n A --> [*]\n B --> [*]");
        assert_eq!(d.nodes.len(), 2, "{:?}", d.nodes);
        // Four arrowheads, and exactly two discs however many exits there are.
        assert_eq!(d.heads.matches('Z').count(), 6, "{:?}", d.heads);
    }

    // --- entity-relationship diagrams ---------------------------------------

    #[test]
    fn an_entity_carries_its_attributes_and_its_cardinality_at_the_ends() {
        let d = flow(
            "erDiagram\n CUSTOMER ||--o{ ORDER : places\n CUSTOMER {\n string name\n string email\n }",
        );
        // CUSTOMER is title + body; ORDER, attribute-less, is one box.
        assert_eq!(d.nodes.len(), 3, "{:?}", d.nodes);
        let texts: Vec<&str> = d.labels.iter().map(|l| l.text.as_str()).collect();
        assert!(
            texts.contains(&"string name") && texts.contains(&"string email"),
            "{texts:?}"
        );
        // The crow's foot is spelled out, because no primitive here draws one.
        assert!(texts.contains(&"1") && texts.contains(&"0..N"), "{texts:?}");
        assert!(texts.contains(&"places"), "{texts:?}");
        assert!(find(&d, "ORDER").y > find(&d, "CUSTOMER").y);
    }

    #[test]
    fn an_er_relationship_is_a_plain_run_with_no_arrowhead_on_either_end() {
        let d = flow("erDiagram\n A ||--|| B : is");
        assert!(
            d.heads.is_empty(),
            "ER relationships are symmetric: {:?}",
            d.heads
        );
        assert!(d.diamonds.is_empty());
        assert!(!d.lines.is_empty());
    }

    #[test]
    fn a_non_identifying_er_relationship_is_the_dashed_one() {
        let d = flow("erDiagram\n A }o..o{ B : maybe");
        assert!(!d.dashed.is_empty(), "{:?}", d.dashed);
        assert!(d.lines.is_empty());
        let texts: Vec<&str> = d.labels.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(
            texts.iter().filter(|t| **t == "0..N").count(),
            2,
            "{texts:?}"
        );
    }

    #[test]
    fn a_hyphenated_entity_name_is_not_read_as_a_relationship() {
        let d = flow("erDiagram\n ORDER ||--|{ LINE-ITEM : contains");
        assert_eq!(d.nodes.len(), 2, "{:?}", d.nodes);
        assert_eq!(find(&d, "LINE-ITEM").text, "LINE-ITEM");
    }

    // --- pie ----------------------------------------------------------------

    #[test]
    fn a_pie_becomes_one_bar_per_slice_scaled_against_the_largest() {
        let d = flow("pie title Pets\n \"Dogs\" : 60\n \"Cats\" : 30\n \"Birds\" : 10");
        assert_eq!(d.nodes.len(), 3, "{:?}", d.nodes);
        // Title, then a name and a percentage for each slice.
        assert_eq!(d.labels[0].text, "Pets");
        let texts: Vec<&str> = d.labels.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["Pets", "Dogs", "60.0%", "Cats", "30.0%", "Birds", "10.0%"]
        );
        // Bars share a left edge and run down the page, longest first here.
        assert_eq!(d.nodes[0].x, d.nodes[1].x);
        assert!(d.nodes[1].y > d.nodes[0].y);
        assert!(d.nodes[0].w > d.nodes[1].w && d.nodes[1].w > d.nodes[2].w);
        assert!(
            (d.nodes[1].w - d.nodes[0].w / 2.0).abs() < 0.5,
            "half of Dogs"
        );
        // Each percentage sits level with the bar it measures.
        for (i, n) in d.nodes.iter().enumerate() {
            let pct = &d.labels[2 + i * 2];
            assert!(pct.x > n.x + n.w, "{pct:?} clears the track");
            assert!((pct.y - n.y).abs() < PIE_ROW_H, "{pct:?} vs {n:?}");
        }
    }

    #[test]
    fn a_pie_row_that_carries_no_number_is_dropped_rather_than_drawn_as_nothing() {
        let d = flow("pie\n \"Real\" : 5\n \"Broken\" : lots");
        assert_eq!(d.nodes.len(), 1, "{:?}", d.nodes);
        assert_eq!(d.labels[0].text, "Real");
    }

    // --- shared invariants --------------------------------------------------

    #[test]
    fn every_dialect_keeps_its_boxes_inside_the_canvas_it_reports() {
        for src in [
            "classDiagram\n class A {\n +x\n }\n A <|-- B\n B *-- C : owns",
            "stateDiagram-v2\n [*] --> A\n A --> B : go\n B --> [*]",
            "erDiagram\n A ||--o{ B : has\n A {\n int id\n }",
            "pie title T\n \"one\" : 1\n \"two\" : 2",
        ] {
            let d = flow(src);
            assert!(d.w > 0.0 && d.h > 0.0, "{src:?}");
            for n in &d.nodes {
                assert!(n.x >= 0.0 && n.y >= 0.0, "{n:?} in {src:?}");
                assert!(
                    n.x + n.w <= d.w + 0.5,
                    "{n:?} overflows w={} in {src:?}",
                    d.w
                );
                assert!(
                    n.y + n.h <= d.h + 0.5,
                    "{n:?} overflows h={} in {src:?}",
                    d.h
                );
            }
        }
    }

    #[test]
    fn a_compartment_stops_listing_members_before_it_becomes_a_wall_of_text() {
        let mut src = String::from("classDiagram\n class Big {\n");
        for i in 0..(MAX_ROWS + 8) {
            src.push_str(&format!(" +field{i}\n"));
        }
        src.push_str(" }\n");
        let d = flow(&src);
        assert_eq!(d.labels.len(), MAX_ROWS);
        assert_eq!(d.labels[MAX_ROWS - 1].text, "…");
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
    fn a_loop_back_to_the_start_does_not_turn_the_diagram_upside_down() {
        // The back edge C -> A is drawn, but it must not rank A below the nodes
        // that feed it: `flowchart TD` means A on top, however it loops.
        let d = flow("flowchart TD\n A --> B\n B --> C\n C --> A");
        assert!(find(&d, "A").y < find(&d, "B").y);
        assert!(find(&d, "B").y < find(&d, "C").y);
    }

    #[test]
    fn the_first_rank_is_never_left_empty_by_a_back_edge() {
        // A [start] whose rank got pushed down used to leave rank 0 unoccupied,
        // opening a band of blank canvas above the whole diagram.
        let d = flow(
            "flowchart TD\n A[start] --> B{ok?}\n B -->|yes| C[ship]\n B -->|no| D[fix]\n D --> A",
        );
        let top = d.nodes.iter().map(|n| n.y).fold(f32::MAX, f32::min);
        assert_eq!(find(&d, "start").y, top);
        assert!(find(&d, "ok?").y > top);
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
