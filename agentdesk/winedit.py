"""Word-style editing for every Text and Entry: Ctrl+Backspace/Delete, Ctrl+A, undo, right-click menu."""

from __future__ import annotations

import tkinter as tk

_ENTRY_CLASSES = ("Entry", "TEntry")


def install(root: tk.Tk) -> None:
    # Load word.tcl first; it resets these variables when it loads lazily.
    root.tk.call("tcl_wordBreakAfter", "", 0)
    root.tk.call("set", "tcl_wordchars", r"\w")
    root.tk.call("set", "tcl_nonwordchars", r"\W")
    root.option_add("*Text.undo", True)
    root.option_add("*Text.autoSeparators", True)

    root.bind_class("Text", "<Control-BackSpace>", _text_delete_prev_word)
    root.bind_class("Text", "<Control-Delete>", _text_delete_next_word)
    for seq in ("<Control-a>", "<Control-A>"):
        root.bind_class("Text", seq, _text_select_all)
    for cls in _ENTRY_CLASSES:
        root.bind_class(cls, "<Control-BackSpace>", _entry_delete_prev_word)
        root.bind_class(cls, "<Control-Delete>", _entry_delete_next_word)
        for seq in ("<Control-a>", "<Control-A>"):
            root.bind_class(cls, seq, _entry_select_all)
    for cls in ("Text",) + _ENTRY_CLASSES:
        root.bind_class(cls, "<Button-3>", _context_menu, add="+")


def _editable(w) -> bool:
    if getattr(w, "_agentdesk_readonly", False):
        return False
    try:
        if w.winfo_class() == "TEntry":
            return not w.instate(["disabled"]) and not w.instate(["readonly"])
        return str(w.cget("state")) == "normal"
    except tk.TclError:
        return False


def _text_delete_prev_word(e):
    w = e.widget
    if not _editable(w):
        return "break"
    if w.tag_ranges("sel"):
        w.delete("sel.first", "sel.last")
        return "break"
    try:
        start = w.tk.call("tk::TextPrevPos", w._w, "insert", "tcl_startOfPreviousWord")
    except tk.TclError:
        start = "insert-1c wordstart"
    w.delete(start, "insert")
    w.see("insert")
    return "break"


def _text_delete_next_word(e):
    w = e.widget
    if not _editable(w):
        return "break"
    if w.tag_ranges("sel"):
        w.delete("sel.first", "sel.last")
        return "break"
    try:
        end = w.tk.call("tk::TextNextWord", w._w, "insert")
    except tk.TclError:
        end = "insert wordend"
    w.delete("insert", end)
    return "break"


def _text_select_all(e):
    w = e.widget
    w.tag_add("sel", "1.0", "end-1c")
    w.mark_set("insert", "end-1c")
    return "break"


def _entry_word(w, forward: bool) -> int:
    ins = w.index("insert")
    ttk_style = w.winfo_class() == "TEntry"
    proc = ("ttk::entry::NextWord" if forward else "ttk::entry::PrevWord") if ttk_style \
        else ("tk::EntryNextWord" if forward else "tk::EntryPreviousWord")
    try:
        return int(w.tk.call(proc, w._w, ins))
    except (tk.TclError, ValueError):
        return len(w.get()) if forward else 0


def _entry_delete_prev_word(e):
    w = e.widget
    if not _editable(w):
        return "break"
    if w.selection_present():
        w.delete("sel.first", "sel.last")
        return "break"
    w.delete(_entry_word(w, False), "insert")
    return "break"


def _entry_delete_next_word(e):
    w = e.widget
    if not _editable(w):
        return "break"
    if w.selection_present():
        w.delete("sel.first", "sel.last")
        return "break"
    w.delete("insert", _entry_word(w, True))
    return "break"


def _entry_select_all(e):
    w = e.widget
    w.selection_range(0, "end")
    w.icursor("end")
    return "break"


def _context_menu(e):
    w = e.widget
    editable = _editable(w)
    try:
        has_sel = bool(w.tag_ranges("sel")) if w.winfo_class() == "Text" else w.selection_present()
    except tk.TclError:
        has_sel = False
    menu = tk.Menu(w, tearoff=0)
    st = lambda ok: "normal" if ok else "disabled"
    if w.winfo_class() == "Text":
        menu.add_command(label="Undo", accelerator="Ctrl+Z", state=st(editable),
                         command=lambda: w.event_generate("<<Undo>>"))
        menu.add_separator()
    menu.add_command(label="Cut", accelerator="Ctrl+X", state=st(editable and has_sel),
                     command=lambda: w.event_generate("<<Cut>>"))
    menu.add_command(label="Copy", accelerator="Ctrl+C", state=st(has_sel),
                     command=lambda: w.event_generate("<<Copy>>"))
    menu.add_command(label="Paste", accelerator="Ctrl+V", state=st(editable),
                     command=lambda: w.event_generate("<<Paste>>"))
    menu.add_separator()
    menu.add_command(label="Select all", accelerator="Ctrl+A",
                     command=lambda: w.event_generate("<Control-a>"))
    try:
        w.focus_set()
        menu.tk_popup(e.x_root, e.y_root)
    finally:
        menu.grab_release()
    return "break"
