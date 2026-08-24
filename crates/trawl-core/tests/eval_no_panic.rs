// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Property test: `eval_expr` RETURNS. That is the whole assertion.
//!
//! The streaming evaluator runs inside an SSE subscription and inside the
//! `rust_stages` batch tail, over values a client chose. A panic there is
//! not a wrong answer — it unwinds a live subscription (or a query task)
//! on one adversarial event, which is a different failure class from the
//! divergences `scalar_parity.rs` hunts. So this test says nothing about
//! WHICH value comes back: the value contract belongs to the parity
//! harness, and duplicating it here would make two owners of one rule.
//!
//! It matters that this runs in the DEBUG profile, where Rust's overflow
//! checks are live: `9223372036854775807 + 1` was a debug panic and a
//! release wrap until #105 made the integer arithmetic checked, and the
//! only build that can observe the difference is this one.
//!
//! Two entry points, because they reach different values. The parsed-DSL
//! path is what a user's query really is; the AST path is the only way to
//! feed the evaluator values the DSL grammar (and JSON) cannot spell —
//! `inf`, `NaN`, `i64::MIN` as a literal rather than a subtraction.

use serde_json::{Map, Value};
use trawl_core::ast::{BinaryOp, Expr, FloatLiteral, LiteralValue, PipeStage, Spanned, UnaryOp};
use trawl_core::eval::eval_expr;
use trawl_core::parser;

// ── Deterministic RNG ────────────────────────────────────────────────
//
// splitmix64, the same generator `tests/scalar_parity.rs` uses, under its
// own seed. Deterministic on purpose: a panic found here has to be
// reproducible from the seed alone.

struct Rng {
    state: u64,
}

impl Rng {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    #[allow(clippy::cast_possible_truncation)]
    fn range(&mut self, n: usize) -> usize {
        (self.next_u64() as usize) % n
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.range(items.len())]
    }
}

// ── The hostile corpus ───────────────────────────────────────────────

/// Integers at the edges where checked arithmetic, negation and `abs`
/// have no answer.
const INTS: &[i64] = &[i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX - 1, i64::MAX];

/// Doubles spanning the IEEE specials, both zeros, and the magnitudes
/// where a cast to `i64` would be undefined.
const FLOATS: &[f64] = &[
    f64::NEG_INFINITY,
    -f64::MAX,
    -1.5,
    -0.0,
    0.0,
    1.5,
    f64::MAX,
    f64::INFINITY,
    f64::NAN,
    f64::MIN_POSITIVE,
];

/// Texts a coercion might reach for: empty, numeric-looking, regex
/// metacharacters, and the hostile timestamp corpus (malformed offsets,
/// impossible components, non-ASCII digits) — every one of them a string
/// some scalar will try to read as a number, a pattern or an instant.
const STRINGS: &[&str] = &[
    "",
    " ",
    "0",
    "-0",
    "infinity",
    "-nan",
    "1e999",
    "9223372036854775808",
    "1_0.0_5",
    "café",
    "日本語",
    "%",
    "_",
    "(",
    "[a-",
    "\\",
    "$.a.b",
    "epoch",
    "2026-01-15 10:20:30",
    "2026-01-15 10:20:30+ab:cd",
    "2026-01-15 10:20:30+99:99",
    "9999-12-31 23:59:59.999999",
    "0000-00-00T00:00:00",
    "٢٠٢٤-٠١-٠١",
    "24:00:00",
    "-9999999999-01-01",
];

/// A string long enough that a length- or index-driven scalar cannot be
/// exercised by the short corpus alone. Kept at 4 KiB rather than
/// megabytes: composition can nest it several deep in one expression,
/// and the cost of a genuinely huge one buys no new code path.
fn huge_string() -> String {
    "übung".repeat(1024)
}

/// The bounds-abusive integers a scalar's second argument can take —
/// `round`'s precision, `substr`'s start and length, `split`'s index.
const ABUSIVE_BOUNDS: &[i64] = &[i64::MIN, -1_000_000, -1, 0, 1, 1_000_000, i64::MAX];

/// Scalars whose arity is 1 and whose argument is the composed
/// subexpression.
const UNARY_SCALARS: &[&str] = &[
    "abs",
    "ceil",
    "floor",
    "round",
    "length",
    "lower",
    "upper",
    "trim",
    "tonumber",
    "tostring",
    "typeof",
    "isnull",
    "json_valid",
    "json_keys",
    "json_array_length",
    "sev",
];

/// The event the expressions read: every value shape a wire event can
/// carry, including the ones `EvalValue::from` maps onto `Null`.
fn hostile_event() -> Map<String, Value> {
    let mut event = Map::new();
    event.insert("service".into(), Value::String("nginx".into()));
    event.insert("empty".into(), Value::String(String::new()));
    event.insert("huge_text".into(), Value::String(huge_string()));
    event.insert("status".into(), Value::Number(i64::MAX.into()));
    event.insert("negative".into(), Value::Number(i64::MIN.into()));
    event.insert(
        "fractional".into(),
        Value::Number(serde_json::Number::from_f64(-1.5).expect("finite")),
    );
    // Beyond i64 AND beyond f64's exact range — `From<&Value>` reads it
    // as a lossy double.
    event.insert(
        "beyond_i64".into(),
        serde_json::from_str::<Value>("9223372036854775808").expect("valid JSON number"),
    );
    event.insert("flag".into(), Value::Bool(true));
    event.insert("missing_value".into(), Value::Null);
    event.insert(
        "list".into(),
        Value::Array(vec![Value::Number(1.into()), Value::String("x".into())]),
    );
    event.insert("nested".into(), serde_json::json!({"a": {"b": [1, 2, 3]}}));
    event.insert("ts".into(), Value::String("2026-01-15 10:20:30".into()));
    event.insert("bad_ts".into(), Value::String("+99:99".into()));
    event
}

/// The field names the generator may reference — the event's own keys
/// plus one it does not carry (an absent field is NULL, and NULL
/// propagation is a code path).
const FIELDS: &[&str] = &[
    "service",
    "empty",
    "huge_text",
    "status",
    "negative",
    "fractional",
    "beyond_i64",
    "flag",
    "missing_value",
    "list",
    "nested",
    "ts",
    "bad_ts",
    "absent_field",
];

// ── AST composition ──────────────────────────────────────────────────

fn spanned(node: Expr) -> Spanned<Expr> {
    Spanned::new(node, 0..0)
}

fn literal(value: LiteralValue) -> Spanned<Expr> {
    spanned(Expr::Literal(value))
}

const BINARY_OPS: &[BinaryOp] = &[
    BinaryOp::Add,
    BinaryOp::Sub,
    BinaryOp::Mul,
    BinaryOp::Div,
    BinaryOp::Mod,
    BinaryOp::Eq,
    BinaryOp::Ne,
    BinaryOp::Gt,
    BinaryOp::Gte,
    BinaryOp::Lt,
    BinaryOp::Lte,
    BinaryOp::And,
    BinaryOp::Or,
    BinaryOp::Matches,
    BinaryOp::Like,
    BinaryOp::ILike,
];

fn leaf(rng: &mut Rng) -> Spanned<Expr> {
    match rng.range(7) {
        0 => literal(LiteralValue::Int(*rng.pick(INTS))),
        1 => {
            let value = *rng.pick(FLOATS);
            // The source token beside the value, as the parser hands one
            // over — `FloatLiteral` carries both.
            literal(LiteralValue::Float(FloatLiteral::new(
                value,
                format!("{value:?}"),
            )))
        }
        2 => literal(LiteralValue::String((*rng.pick(STRINGS)).to_string())),
        3 => literal(LiteralValue::String(huge_string())),
        4 => literal(LiteralValue::Bool(rng.range(2) == 0)),
        5 => literal(LiteralValue::Null),
        6 => spanned(Expr::FieldRef((*rng.pick(FIELDS)).to_string())),
        _ => unreachable!(),
    }
}

/// One randomly composed expression, at most `depth` operators deep.
fn compose(rng: &mut Rng, depth: usize) -> Spanned<Expr> {
    if depth == 0 {
        return leaf(rng);
    }
    match rng.range(9) {
        0 => leaf(rng),
        1 => spanned(Expr::Binary {
            lhs: Box::new(compose(rng, depth - 1)),
            op: *rng.pick(BINARY_OPS),
            rhs: Box::new(compose(rng, depth - 1)),
        }),
        2 => spanned(Expr::Unary {
            op: if rng.range(2) == 0 {
                UnaryOp::Neg
            } else {
                UnaryOp::Not
            },
            operand: Box::new(compose(rng, depth - 1)),
        }),
        3 => {
            let items = (0..rng.range(4)).map(|_| compose(rng, depth - 1)).collect();
            spanned(Expr::InList {
                expr: Box::new(compose(rng, depth - 1)),
                list: items,
            })
        }
        4 => call(rng.pick(UNARY_SCALARS), vec![compose(rng, depth - 1)]),
        // Bounds-abusive second (and third) arguments: the window
        // arithmetic in `substr`, `round`'s precision, `split`'s index.
        5 => call(
            "round",
            vec![
                compose(rng, depth - 1),
                literal(LiteralValue::Int(*rng.pick(ABUSIVE_BOUNDS))),
            ],
        ),
        6 => {
            let mut args = vec![
                compose(rng, depth - 1),
                literal(LiteralValue::Int(*rng.pick(ABUSIVE_BOUNDS))),
            ];
            if rng.range(2) == 0 {
                args.push(literal(LiteralValue::Int(*rng.pick(ABUSIVE_BOUNDS))));
            }
            call("substr", args)
        }
        7 => {
            let name = *rng.pick(&["date_part", "date_trunc", "strftime", "strptime", "json"]);
            let unit = *rng.pick(&[
                "epoch",
                "year",
                "week",
                "second",
                "%Y-%m-%d %H:%M:%S",
                "%f",
                "$.a.b",
                "not-a-unit",
            ]);
            // `strftime`/`json` take the value first, the rest take it
            // second — both orders reach a different guard.
            let args = if matches!(name, "strftime" | "strptime" | "json") {
                vec![
                    compose(rng, depth - 1),
                    literal(LiteralValue::String(unit.to_string())),
                ]
            } else {
                vec![
                    literal(LiteralValue::String(unit.to_string())),
                    compose(rng, depth - 1),
                ]
            };
            call(name, args)
        }
        8 => {
            let name = *rng.pick(&["if", "case", "coalesce", "concat", "date_diff", "split"]);
            let arity = 1 + rng.range(3);
            let args = (0..arity).map(|_| compose(rng, depth - 1)).collect();
            call(name, args)
        }
        _ => unreachable!(),
    }
}

fn call(name: &str, args: Vec<Spanned<Expr>>) -> Spanned<Expr> {
    spanned(Expr::FunctionCall {
        name: name.to_string(),
        args,
    })
}

#[test]
fn eval_returns_for_every_composed_expression() {
    let event = trawl_core::row::from_json(&hostile_event());
    let mut rng = Rng::new(0x0105_A57C_0DE0);
    for _ in 0..4_000 {
        let expression = compose(&mut rng, 4);
        // The ONLY assertion: control comes back. Which value it carries
        // is `scalar_parity.rs`'s question, not this test's.
        let _answer = eval_expr(&expression, &event);
    }
}

// ── The parsed-DSL path ──────────────────────────────────────────────

/// Literal tokens the DSL grammar accepts, chosen to reach the same
/// edges the AST corpus does through real source text.
const DSL_ATOMS: &[&str] = &[
    "9223372036854775807",
    "-9223372036854775807",
    "0",
    "-1",
    "1.5",
    "-0.0",
    "0.0",
    "true",
    "false",
    "null",
    "status",
    "negative",
    "beyond_i64",
    "missing_value",
    "absent_field",
    "huge_text",
    "ts",
    "bad_ts",
    "list",
    "nested",
    r#""""#,
    r#""infinity""#,
    r#""2026-01-15 10:20:30+ab:cd""#,
    r#""[a-""#,
];

const DSL_OPS: &[&str] = &[
    "+", "-", "*", "/", "%", "==", "!=", ">", ">=", "<", "<=", "and", "or", "matches", "like",
];

/// A DSL expression built from the atoms above, at most `depth` deep.
fn compose_dsl(rng: &mut Rng, depth: usize) -> String {
    if depth == 0 {
        return (*rng.pick(DSL_ATOMS)).to_string();
    }
    match rng.range(6) {
        0 => (*rng.pick(DSL_ATOMS)).to_string(),
        1 => format!(
            "({} {} {})",
            compose_dsl(rng, depth - 1),
            rng.pick(DSL_OPS),
            compose_dsl(rng, depth - 1)
        ),
        2 => format!("not ({})", compose_dsl(rng, depth - 1)),
        3 => format!("-({})", compose_dsl(rng, depth - 1)),
        4 => format!(
            "{}({})",
            rng.pick(UNARY_SCALARS),
            compose_dsl(rng, depth - 1)
        ),
        5 => format!(
            "{}({}, {})",
            rng.pick(&["round", "substr", "date_diff", "coalesce", "concat"]),
            compose_dsl(rng, depth - 1),
            rng.pick(ABUSIVE_BOUNDS)
        ),
        _ => unreachable!(),
    }
}

#[test]
fn eval_returns_for_every_parsed_dsl_expression() {
    let event = trawl_core::row::from_json(&hostile_event());
    let mut rng = Rng::new(0x0105_D51C_0DE0);
    let mut evaluated = 0_u32;
    for _ in 0..4_000 {
        let dsl = format!("* | let x = {}", compose_dsl(&mut rng, 3));
        // A rejected generated string is a PARSE outcome, not a panic —
        // this generator is deliberately loose about types and arities,
        // which the parser and the pipeline are entitled to refuse.
        let Ok(query) = parser::parse(&dsl) else {
            continue;
        };
        let Some(PipeStage::Let(stage)) = query.pipeline.first().map(|stage| &stage.node) else {
            panic!("generated query has no let stage: {dsl:?}");
        };
        let (_, expression) = stage
            .assignments
            .first()
            .unwrap_or_else(|| panic!("generated let stage has no assignment: {dsl:?}"));
        let _answer = eval_expr(expression, &event);
        evaluated += 1;
    }
    // A generator that stopped parsing anything would pass the loop above
    // vacuously.
    assert!(
        evaluated > 1_000,
        "only {evaluated} of 4000 generated DSL expressions parsed — the \
         generator, not the evaluator, is what this run measured"
    );
}
