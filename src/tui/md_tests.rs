use super::*;

#[test]
fn inline_marks_up_what_it_should_and_leaves_the_rest() {
    let (bold, strike, italic) = (with("fg", "b"), with("fg", "s"), with("fg", "i"));
    let cases: Vec<(&str, L)> = vec![
        ("a **b** c", vec![s("a ", "fg"), s("b", &bold), s(" c", "fg")]),
        ("~~x~~", vec![s("x", &strike)]),
        ("`code` *it*", vec![s("code", "cy"), s(" ", "fg"), s("it", &italic)]),
        ("* not* italic", vec![s("* not* italic", "fg")]),
        ("[go](http://a.b) x", vec![s("go", "cy u"), s(" x", "fg")]),
        ("see https://a.b/c, ok", vec![s("see ", "fg"), s("https://a.b/c", "cy u"), s(", ok", "fg")]),
        ("**a `b ~~c [d](", vec![s("**a `b ~~c [d](", "fg")]),
        ("é **日本** é", vec![s("é ", "fg"), s("日本", &bold), s(" é", "fg")]),
    ];
    for (text, want) in cases {
        assert_eq!(inline(text, "fg"), want, "{text:?}");
    }
}

#[test]
fn a_long_paragraph_renders_in_linear_time() {
    let para = "word **bold** and `code` and https://example.com/x, [link](http://a.b) ".repeat(400);
    let t = std::time::Instant::now();
    let lines = render(&para, 100);
    assert!(!lines.is_empty());
    assert!(t.elapsed() < std::time::Duration::from_secs(2), "took {:?}", t.elapsed());
}
