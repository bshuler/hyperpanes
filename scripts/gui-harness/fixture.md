# Hyperpanes rendering fixture

This file exists to be looked at, not read. Every construct the markdown preview
knows how to draw appears below at least once, and the prose paragraphs are
deliberately long enough to wrap two or three times at the widths the harness
uses — wrapping is the whole point, because a wrapped line is the only place
where the renderer's own leading shows up instead of the row padding that sits
between one block and the next.

## Paragraphs that wrap

The rhythm of a page is set by the distance between one baseline and the next.
When that distance is the same everywhere, the eye stops noticing it and reads
the words instead. When a wrapped continuation sits closer to its predecessor
than two neighbouring blocks sit to each other, the continuation reads as though
it has crept upward into the line above, and the page acquires a stutter that no
individual line is responsible for.

A second paragraph, so the gap between two paragraphs can be compared against
the gap inside one. This one also runs long enough to wrap, which is what makes
the comparison possible at all: a single-line paragraph tells you nothing about
internal leading.

## Lists

- A short item.
- A much longer list item that will certainly wrap at any sensible pane width,
  because the interesting question for a list is whether its continuation lines
  keep the same rhythm as its first lines do.
- Another short item.

1. Ordinals get a marker in the gutter.
2. The marker must sit on the item's *first* line, not float to the middle of a
   wrapped item, which is why it is positioned against the text and not against
   the row box.
3. Third.

## Quote and code

> A block quote, drawn in the dimmer ink, and long enough to wrap so its own
> internal leading can be compared with everything else on the page.

```rust
fn pitch(ascent: f32, descent: f32, leading: f32, upem: f32) -> f32 {
    (ascent - descent + leading) / upem
}
```

## Headings and rules

### A third-level heading

Text under the third-level heading, again long enough to wrap so that the space
a heading claims above and below itself can be judged against the body rhythm.

---

A closing paragraph after the horizontal rule, with a [link](https://example.com)
and some `inline code` in it, both of which change colour without changing the
line box they sit in.
