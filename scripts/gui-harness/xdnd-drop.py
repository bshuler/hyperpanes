#!/usr/bin/env python3
"""Perform a real XDND drop of one or more files onto a window.

    xdnd-drop.py --x 400 --y 300 /path/to/file [more files...]

This is the harness's stand-in for a human dragging a file out of a file manager
and letting go over a pane. It is a genuine X11 drag: the target sees XdndEnter,
XdndPosition, XdndDrop, asks for the selection, and gets a `text/uri-list` back
over the wire — the same conversation a real file manager has. Nothing about it
is a shortcut past the app's drop handling.

Debian has no `dragon-drop` in bookworm, and a scripted source is better here
anyway: the harness needs to control exactly which mime types are offered, so a
failure can be pinned on the app rather than on a helper's choices.

Exit 0 means the target accepted the drop and reported XdndFinished.
"""
import argparse
import os
import sys
import time
from urllib.parse import quote

from Xlib import X, display, Xatom
from Xlib.protocol import event

XDND_VERSION = 5


def atoms(d):
    names = [
        "XdndAware", "XdndSelection", "XdndEnter", "XdndPosition", "XdndStatus",
        "XdndDrop", "XdndFinished", "XdndLeave", "XdndActionCopy", "XdndTypeList",
        "text/uri-list",
    ]
    return {n: d.intern_atom(n) for n in names}


def find_xdnd_target(d, a, root, x, y):
    """Walk down from the root to the deepest window at (x, y) that is XdndAware.

    The app's drop target is not necessarily the top-level window, and it is not
    necessarily the deepest child either — so remember the last aware window seen
    on the way down rather than insisting on either end.
    """
    win = root
    best = None
    for _ in range(32):
        if win.get_full_property(a["XdndAware"], X.AnyPropertyType) is not None:
            best = win
        tr = win.translate_coords(win, x, y)
        child = tr.child
        if not child:
            break
        win = child
    return best


class Source:
    def __init__(self, d, a, uris):
        self.d, self.a = d, a
        self.data = ("".join(u + "\r\n" for u in uris)).encode()
        screen = d.screen()
        # An unmapped 1x1 window is enough: a drag source needs an identity and a
        # selection owner, not pixels.
        self.win = screen.root.create_window(
            0, 0, 1, 1, 0, screen.root_depth,
            X.InputOutput, X.CopyFromParent,
            event_mask=X.PropertyChangeMask,
        )
        self.win.set_wm_name("hyperpanes-xdnd-source")
        self.win.change_property(a["XdndAware"], Xatom.ATOM, 32, [XDND_VERSION])

    def own_selection(self, t):
        self.win.set_selection_owner(self.a["XdndSelection"], t)
        self.d.sync()
        return self.d.get_selection_owner(self.a["XdndSelection"]) == self.win

    def send(self, target, msg, l):
        target.send_event(
            event.ClientMessage(window=target, client_type=self.a[msg],
                                data=(32, (l + [0, 0, 0, 0, 0])[:5])),
            event_mask=X.NoEventMask,
        )
        self.d.flush()

    def serve(self, ev):
        """Answer a SelectionRequest for XdndSelection with the uri-list."""
        req = ev.requestor
        prop = ev.property or ev.target
        if ev.target == self.a["text/uri-list"]:
            req.change_property(prop, ev.target, 8, self.data)
        else:
            prop = X.NONE  # refuse anything we did not offer
        req.send_event(
            event.SelectionNotify(time=ev.time, requestor=req, selection=ev.selection,
                                  target=ev.target, property=prop),
            event_mask=X.NoEventMask,
        )
        self.d.flush()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--x", type=int, required=True)
    ap.add_argument("--y", type=int, required=True)
    ap.add_argument("--timeout", type=float, default=5.0)
    ap.add_argument("files", nargs="+")
    args = ap.parse_args()

    uris = []
    for f in args.files:
        p = os.path.abspath(f)
        if not os.path.exists(p):
            print(f"no such file: {p}", file=sys.stderr)
            return 2
        uris.append("file://" + quote(p))

    d = display.Display()
    a = atoms(d)
    root = d.screen().root

    target = find_xdnd_target(d, a, root, args.x, args.y)
    if target is None:
        print(f"no XdndAware window at ({args.x}, {args.y})", file=sys.stderr)
        return 1

    src = Source(d, a, uris)
    now = int(time.time()) & 0xFFFFFFFF
    if not src.own_selection(X.CurrentTime):
        print("could not take ownership of XdndSelection", file=sys.stderr)
        return 1

    src.send(target, "XdndEnter",
             [src.win.id, XDND_VERSION << 24, a["text/uri-list"], 0, 0])
    src.send(target, "XdndPosition",
             [src.win.id, 0, (args.x << 16) | args.y, X.CurrentTime, a["XdndActionCopy"]])

    accepted = None
    finished = False
    dropped = False
    deadline = time.time() + args.timeout
    while time.time() < deadline and not finished:
        if d.pending_events() == 0:
            time.sleep(0.02)
            continue
        ev = d.next_event()
        if ev.type == X.SelectionRequest:
            src.serve(ev)
        elif ev.type == X.ClientMessage:
            if ev.client_type == a["XdndStatus"] and not dropped:
                accepted = bool(ev.data[1][1] & 1)
                if not accepted:
                    break
                src.send(target, "XdndDrop", [src.win.id, 0, X.CurrentTime])
                dropped = True
            elif ev.client_type == a["XdndFinished"]:
                finished = True

    if accepted is None:
        print("target never answered with XdndStatus", file=sys.stderr)
        return 1
    if not accepted:
        print("target refused the drop (XdndStatus accept=0)", file=sys.stderr)
        return 1
    if not finished:
        print("target accepted but never sent XdndFinished", file=sys.stderr)
        return 1
    print(f"dropped {len(uris)} uri(s) on window 0x{target.id:x}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
