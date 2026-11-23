//! Money chat plugins (Mercury banking, CoinPay crypto payments, moomoo
//! brokerage) on the CLI side: the read-only allowlists, the approval card a
//! held call shows, and the rule that nothing but an explicit `y` at a
//! terminal approves a money action (`--yes` and piped input never do).
//!
//! The gate itself runs on the server (c0mpute.com holds every tool on a
//! money plugin that is not on its read-only allowlist, unknown names
//! included). The allowlists below are the same ones, embedded from the
//! `@profullstack/chat-plugins` package as `money_policies.json` (regenerate
//! with `node scripts/gen-money-policies.mjs`; a web test fails on drift).
//! The CLI uses them to tell a money call from any other held call, and to
//! build the card itself when the server sent no `summary`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::{Map, Value};

pub const MODEL_CAN_BE_WRONG: &str = "The model can be wrong. You decide: nothing here runs until you approve it.";
const MERCURY_NOTE: &str = "Mercury payment and transfer requests still need an approver in Mercury.";

#[derive(Debug, Deserialize)]
pub struct Policy {
    #[serde(skip)]
    pub slug: String,
    /// "banking" | "crypto" | "trading"
    pub kind: String,
    pub hosts: Vec<String>,
    pub read_only: BTreeSet<String>,
    #[serde(default)]
    pub paper_writes: BTreeSet<String>,
}

impl Policy {
    /// A tool runs without asking only when its exact name is allowlisted.
    pub fn needs_confirmation(&self, tool: &str) -> bool {
        !self.read_only.contains(tool)
    }

    /// Whether the server lists the tool at all in this trading mode (paper
    /// hides live order tools).
    pub fn visible_in_mode(&self, tool: &str, mode: &str) -> bool {
        self.kind != "trading" || mode == "live" || self.read_only.contains(tool) || self.paper_writes.contains(tool)
    }
}

pub fn policies() -> &'static BTreeMap<String, Policy> {
    static P: OnceLock<BTreeMap<String, Policy>> = OnceLock::new();
    P.get_or_init(|| {
        let mut m: BTreeMap<String, Policy> =
            serde_json::from_str(include_str!("money_policies.json")).expect("money_policies.json is valid");
        for (slug, p) in m.iter_mut() {
            p.slug = slug.clone();
        }
        m
    })
}

/// The policy for a plugin's server URL, chosen by host (as the server does).
pub fn policy_for_url(url: &str) -> Option<&'static Policy> {
    let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or("");
    let host = host.split(':').next().unwrap_or("").trim_end_matches('.').to_ascii_lowercase();
    policies().values().find(|p| p.hosts.iter().any(|h| *h == host))
}

/// The policy a catalog slug names (used when the URL is not known).
pub fn policy_for_slug(slug: &str) -> Option<&'static Policy> {
    policies().get(slug)
}

/// The approval card's content: the server's `summary` or one built here.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub title: String,
    pub lines: Vec<(String, String)>,
    /// "paper" | "live" for orders.
    pub account: Option<String>,
    pub note: String,
}

impl Summary {
    pub fn from_json(v: &Value) -> Option<Summary> {
        let title = v.get("title")?.as_str()?.to_string();
        let lines = v
            .get("lines")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|l| Some((l.get("label")?.as_str()?.to_string(), value_str(l.get("value")?))))
                    .collect()
            })
            .unwrap_or_default();
        Some(Summary {
            title,
            lines,
            account: v.get("account").and_then(Value::as_str).map(str::to_string),
            note: v.get("note").and_then(Value::as_str).unwrap_or(MODEL_CAN_BE_WRONG).to_string(),
        })
    }
}

/// A plugin call the server held for approval.
#[derive(Debug, Clone)]
pub struct Held {
    pub plugin: String,
    pub tool: String,
    pub arguments: Value,
    /// Signed approval token for exactly this call.
    pub token: String,
    pub summary: Option<Summary>,
    /// A money action: card + explicit `y` at a terminal, never `--yes`.
    pub money: bool,
}

impl Held {
    /// From an `approval_needed` plugin event; None without a token.
    pub fn from_event(e: &Value) -> Option<Held> {
        let token = e.get("approval")?.as_str()?.to_string();
        let plugin = e["plugin"].as_str().unwrap_or("?").to_string();
        let summary = e.get("summary").and_then(Summary::from_json);
        // The server only attaches a summary to money plugins; a catalog slug
        // is enough to treat it as money until the URL says otherwise.
        let money = summary.is_some() || policy_for_slug(&plugin).is_some();
        Some(Held {
            tool: e["tool"].as_str().unwrap_or("?").to_string(),
            arguments: e.get("arguments").cloned().unwrap_or(Value::Object(Map::new())),
            token,
            summary,
            money,
            plugin,
        })
    }

    /// Settle `money` (and fill in a card) once the plugin's URL is looked
    /// up. `url` None means the lookup failed: assume money, never less safe.
    pub fn classify(&mut self, url: Option<&str>) {
        let policy = match url {
            Some(u) => policy_for_url(u),
            None => {
                self.money = true;
                policy_for_slug(&self.plugin)
            }
        };
        if let Some(p) = policy {
            self.money = true;
            if self.summary.is_none() {
                self.summary = Some(describe_action(p, &self.tool, &self.arguments));
            }
        }
    }

    /// "plugin · tool {args}", for the one-line prompt of a non-money call.
    pub fn describe(&self) -> String {
        let args = self.arguments.to_string();
        let args = if args.chars().count() > 200 { format!("{}…", args.chars().take(200).collect::<String>()) } else { args };
        format!("{} · {} {}", self.plugin, self.tool, if args == "{}" { String::new() } else { args })
    }
}

/// What to do with one held call.
#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    /// Approved without asking (`--yes`, non-money only).
    Approve,
    /// Show it and wait for an explicit `y`.
    Ask,
    /// Not approved; the reason is printed.
    Decline(&'static str),
}

pub const MONEY_NEEDS_TERMINAL: &str =
    "not approved: a money action needs an explicit y typed at a terminal (--yes and piped input never approve one)";
pub const NEEDS_TERMINAL: &str = "not approved: no terminal to ask at (pass --yes to approve tool calls that move no money)";

/// `--yes` approves held calls that move no money; a money call is only ever
/// asked about, and only at a terminal.
pub fn plan(h: &Held, yes: bool, interactive: bool) -> Plan {
    match (h.money, interactive, yes) {
        (true, true, _) => Plan::Ask,
        (true, false, _) => Plan::Decline(MONEY_NEEDS_TERMINAL),
        (false, _, true) => Plan::Approve,
        (false, true, false) => Plan::Ask,
        (false, false, false) => Plan::Decline(NEEDS_TERMINAL),
    }
}

/// Only an explicit yes approves; Enter, "n" or anything else declines.
pub fn accepts(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// The prompt shown after the card (or for a plain call).
pub fn prompt(h: &Held) -> String {
    if h.money {
        "approve? type y to run it, anything else declines [y/N] ".to_string()
    } else {
        format!("approve {}? [y/N] ", h.describe())
    }
}

/// The approval card, the same fields the web chat shows: payments (amount,
/// currency, from, to, memo) or orders (symbol, side, quantity, order type,
/// limit/stop, estimated cost, account, PAPER or LIVE).
pub fn render_card(h: &Held) -> String {
    let fallback;
    let s = match &h.summary {
        Some(s) => s,
        None => {
            fallback = Summary { title: humanize_tool(&h.tool), lines: Vec::new(), account: None, note: MODEL_CAN_BE_WRONG.into() };
            &fallback
        }
    };
    let head = format!("approval needed · {} · {}", h.plugin, h.tool);
    let mut out = String::new();
    out.push_str(&format!("┌─ {head}\n"));
    let banner = match s.account.as_deref() {
        Some("live") => "  [LIVE: real money]",
        Some("paper") => "  [PAPER: simulated]",
        _ => "",
    };
    out.push_str(&format!("│ {}{banner}\n", s.title));
    let width = s.lines.iter().map(|(l, _)| l.chars().count()).max().unwrap_or(0);
    for (label, value) in &s.lines {
        let pad = " ".repeat(width - label.chars().count());
        out.push_str(&format!("│   {label}{pad}  {value}\n"));
    }
    if s.lines.is_empty() {
        let args = h.arguments.to_string();
        let args: String = args.chars().take(300).collect();
        out.push_str(&format!("│   arguments  {args}\n"));
    }
    out.push_str(&format!("│ {}\n", s.note));
    out.push_str("└─");
    out
}

// ── describeAction, ported from @profullstack/chat-plugins src/money.js ──

pub fn describe_action(policy: &Policy, tool: &str, args: &Value) -> Summary {
    let empty = Value::Object(Map::new());
    let a = if args.is_object() { args } else { &empty };
    if policy.kind == "trading" { trading_summary(tool, a) } else { money_summary(&policy.kind, tool, a) }
}

fn value_str(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

fn norm(k: &str) -> String {
    k.to_lowercase().replace(['_', '-'], "")
}

/// Keys that hold an adjustment to an amount rather than the amount itself.
fn is_adjustment_key(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    ["discount", "tax", "fee", "tip", "coupon", "rebate", "credit", "adjust", "shipping"].iter().any(|w| k.contains(w))
}

/// The first non-empty value among `names` (case, `_` and `-` ignored), at
/// the top level, else one object level down.
fn pick(args: &Value, names: &[&str]) -> String {
    let want: Vec<String> = names.iter().map(|n| norm(n)).collect();
    let scan = |o: &Map<String, Value>| -> String {
        for w in &want {
            for (k, v) in o {
                if norm(k) == *w && !v.is_null() && v.as_str() != Some("") {
                    return value_str(v);
                }
            }
        }
        String::new()
    };
    let Some(obj) = args.as_object() else { return String::new() };
    let top = scan(obj);
    if !top.is_empty() {
        return top;
    }
    // One level down for wrapped arguments, never into a discount, tax or fee
    // object: its `value`/`amount` adjusts the amount, it is not the amount.
    for (k, v) in obj {
        if is_adjustment_key(k) {
            continue;
        }
        if let Some(inner) = v.as_object() {
            let s = scan(inner);
            if !s.is_empty() {
                return s;
            }
        }
    }
    String::new()
}

pub fn humanize_tool(tool: &str) -> String {
    let mut s = String::new();
    let mut prev_lower = false;
    for c in tool.chars() {
        if c.is_ascii_uppercase() && prev_lower {
            s.push(' ');
        }
        prev_lower = c.is_ascii_lowercase();
        s.push(if c == '_' { ' ' } else { c });
    }
    let s = s.to_lowercase();
    let mut cs = s.chars();
    match cs.next() {
        Some(f) => f.to_uppercase().collect::<String>() + cs.as_str(),
        None => s,
    }
}

fn num(s: &str) -> Option<f64> {
    let t = s.replace(',', "");
    let t = t.trim();
    if t.is_empty() {
        return None;
    }
    t.parse::<f64>().ok().filter(|n| n.is_finite())
}

/// `n.toLocaleString("en-US", { maximumFractionDigits })`.
pub fn format_num(n: f64, max_frac: usize) -> String {
    let fixed = format!("{:.*}", max_frac, n.abs());
    let (int, frac) = fixed.split_once('.').unwrap_or((&fixed, ""));
    let frac = frac.trim_end_matches('0');
    let mut grouped = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    let neg = n < 0.0 && (grouped != "0" || !frac.is_empty());
    format!("{}{grouped}{}{frac}", if neg { "-" } else { "" }, if frac.is_empty() { "" } else { "." })
}

fn trading_summary(tool: &str, args: &Value) -> Summary {
    let paper = tool.starts_with("sim_trade_");
    let mut lines: Vec<(String, String)> = Vec::new();
    let add = |lines: &mut Vec<(String, String)>, label: &str, value: String| {
        if !value.is_empty() {
            lines.push((label.into(), value));
        }
    };
    let is_cancel = tool.contains("cancel");
    let is_change = tool.contains("modify") || tool.contains("replace");
    let is_place = !is_cancel && !is_change && (tool.contains("order") || tool.contains("place") || tool.contains("confirm"));
    let title = if is_cancel {
        "Cancel order".to_string()
    } else if is_change {
        "Change order".to_string()
    } else if tool.contains("confirm") {
        "Confirm order".to_string()
    } else if is_place {
        "Place order".to_string()
    } else {
        humanize_tool(tool)
    };
    add(&mut lines, "Symbol", pick(args, &["code", "symbol", "ticker", "security"]));
    add(&mut lines, "Side", pick(args, &["side", "trd_side", "direction"]));
    let qty = pick(args, &["qty", "quantity", "new_qty", "shares"]);
    add(&mut lines, "Quantity", qty.clone());
    let typ = pick(args, &["order_type", "type"]);
    add(&mut lines, "Order type", typ.clone());
    let price = pick(args, &["price", "limit_price", "new_price"]);
    add(&mut lines, "Limit price", price.clone());
    add(&mut lines, "Stop / trigger price", pick(args, &["aux_price", "stop_price", "trigger_price"]));
    add(&mut lines, "Time in force", pick(args, &["time_in_force", "tif"]));
    add(&mut lines, "Order id", pick(args, &["order_id", "confirm_id"]));
    if is_place || is_change {
        let value = match (num(&qty), num(&price)) {
            (Some(q), Some(p)) if !typ.to_ascii_uppercase().contains("MARKET") => format!(
                "{} (quantity x price, before fees; options and futures also multiply by the contract size)",
                format_num(q * p, 4)
            ),
            _ => "at the market price, not known until it fills".to_string(),
        };
        lines.push(("Estimated cost".into(), value));
    }
    add(&mut lines, "Account id", pick(args, &["acc_id", "account_id"]));
    lines.push((
        "Account".into(),
        if paper { "PAPER (simulated, no real money)" } else { "LIVE (real money)" }.to_string(),
    ));
    Summary { title, lines, account: Some(if paper { "paper" } else { "live" }.into()), note: MODEL_CAN_BE_WRONG.into() }
}

pub const AMOUNT_SEE_DETAILS: &str = "see details";

/// An itemised invoice's totals, in cents.
#[derive(Debug, PartialEq)]
pub struct InvoiceTotals {
    pub subtotal: i128,
    pub discount: i128,
    pub discount_label: String,
    pub tax: i128,
    pub taxed_lines: usize,
    pub total: i128,
    pub lines: usize,
}

fn to_num(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64().filter(|f| f.is_finite()),
        Value::String(s) if !s.trim().is_empty() => num(s),
        _ => None,
    }
}

fn cents(n: f64) -> i128 {
    (n * 100.0).round() as i128
}

/// Half away from zero, for non-negative a and positive b.
fn div_round(a: i128, b: i128) -> i128 {
    (a * 2 + b) / (b * 2)
}

fn money_str(c: i128) -> String {
    format_num(c as f64 / 100.0, 2)
}

/// CoinPay's invoice arithmetic (computeInvoiceTotals) in integer cents, as
/// in @profullstack/chat-plugins invoiceTotals(): subtotal of quantity x
/// unit_price, the discount (percent of the subtotal or fixed), each line's
/// tax_rate on its share of the discounted subtotal. Err when the arguments
/// do not pin the total down.
pub fn invoice_totals(items: &Value, discount: Option<&Value>) -> Result<InvoiceTotals, String> {
    let items = items.as_array().filter(|a| !a.is_empty()).ok_or("no line items")?;
    let mut lines: Vec<(i128, f64)> = Vec::new();
    for (i, it) in items.iter().enumerate() {
        let n = i + 1;
        let o = it.as_object().ok_or(format!("line {n} is not an item"))?;
        let q = to_num(o.get("quantity")).filter(|q| *q >= 0.0).ok_or(format!("line {n} has no usable quantity"))?;
        let p = to_num(o.get("unit_price").or_else(|| o.get("unitPrice")))
            .filter(|p| *p >= 0.0)
            .ok_or(format!("line {n} has no usable unit_price"))?;
        let rate = match o.get("tax_rate").or_else(|| o.get("taxRate")) {
            None | Some(Value::Null) => 0.0,
            Some(Value::String(s)) if s.is_empty() => 0.0,
            r => to_num(r).filter(|r| *r >= 0.0).ok_or(format!("line {n} has an unusable tax_rate"))?,
        };
        lines.push((div_round((q * 1e4).round() as i128 * cents(p), 10_000), rate));
    }
    let subtotal: i128 = lines.iter().map(|l| l.0).sum();
    let (mut off, mut discount_label) = (0i128, String::new());
    if let Some(d) = discount.filter(|d| !d.is_null()) {
        let typ = d.get("type").and_then(Value::as_str);
        let value = to_num(d.get("value")).filter(|v| *v >= 0.0);
        let (Some(typ @ ("percent" | "fixed")), Some(value)) = (typ, value) else {
            return Err("the discount is not {type: percent|fixed, value}".into());
        };
        if typ == "percent" && value > 100.0 {
            return Err("the discount is over 100%".into());
        }
        off = if typ == "percent" { div_round(subtotal * (value * 1000.0).round() as i128, 100_000) } else { cents(value) };
        if off > subtotal {
            return Err("the discount is larger than the subtotal".into());
        }
        discount_label = if typ == "percent" {
            format!("{}% = -{}", format_num(value, 6), money_str(off))
        } else {
            format!("fixed -{}", money_str(off))
        };
    }
    let mut share = vec![0i128; lines.len()];
    if off > 0 {
        let (mut given, mut last) = (0i128, None);
        for (i, l) in lines.iter().enumerate() {
            if l.0 == 0 {
                continue;
            }
            last = Some(i);
            share[i] = l.0 * off / subtotal;
            given += share[i];
        }
        if let Some(i) = last {
            share[i] += off - given;
        }
    }
    let tax: i128 = lines
        .iter()
        .zip(&share)
        .map(|(l, s)| if l.1 > 0.0 { div_round((l.0 - s) * (l.1 * 1000.0).round() as i128, 100_000) } else { 0 })
        .sum();
    let taxed_lines = lines.iter().filter(|l| l.1 > 0.0).count();
    Ok(InvoiceTotals { subtotal, discount: off, discount_label, tax, taxed_lines, total: subtotal - off + tax, lines: lines.len() })
}

fn clip(s: String, max: usize) -> String {
    if s.chars().count() > max { format!("{}…", s.chars().take(max - 1).collect::<String>()) } else { s }
}

fn describe_lines(items: &Value) -> String {
    let parts: Vec<String> = items
        .as_array()
        .map(|a| {
            a.iter()
                .map(|it| {
                    let d = value_str(it.get("description").or_else(|| it.get("name")).unwrap_or(&Value::Null));
                    let price = it.get("unit_price").or_else(|| it.get("unitPrice")).unwrap_or(&Value::Null);
                    let tax = match it.get("tax_rate") {
                        Some(t) if !t.is_null() && t != &Value::from(0) && value_str(t) != "0" && value_str(t) != "" => format!(" (+{}% tax)", value_str(t)),
                        _ => String::new(),
                    };
                    format!(
                        "{} x {}{}{tax}",
                        value_str(it.get("quantity").unwrap_or(&Value::Null)),
                        value_str(price),
                        if d.is_empty() { String::new() } else { format!(" {d}") }
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    clip(parts.join("; "), 300)
}

fn money_summary(kind: &str, tool: &str, args: &Value) -> Summary {
    let mut lines: Vec<(String, String)> = Vec::new();
    let add = |lines: &mut Vec<(String, String)>, label: &str, value: String| {
        if !value.is_empty() {
            lines.push((label.into(), value));
        }
    };
    let given = pick(args, &["amount", "amountUsd", "value", "total", "paymentAmount"]);
    let mut currency = pick(args, &["currency", "currencyCode", "asset", "coin"]);
    if let Some(m) = args.get("minutes").filter(|m| m.is_number()) {
        add(&mut lines, "Time", format!("{} minutes", value_str(m)));
    }
    if let Some(items) = args.get("lineItems").or_else(|| args.get("line_items")) {
        // An itemised invoice: the amount is what the items add up to, never
        // a number found elsewhere in the arguments.
        let discount = args.get("discount");
        let t = invoice_totals(items, discount).and_then(|t| match (given.is_empty(), num(&given)) {
            (true, _) => Ok(t),
            (false, Some(g)) if cents(g) == t.total => Ok(t),
            _ => Err(format!("the amount given ({given}) does not match the line items")),
        });
        match t {
            Err(why) => {
                add(&mut lines, "Amount", format!("{AMOUNT_SEE_DETAILS} ({why})"));
                add(&mut lines, "Line items", clip(items.to_string(), 500));
                if let Some(d) = discount {
                    add(&mut lines, "Discount", clip(d.to_string(), 500));
                }
            }
            Ok(t) => {
                let n = t.lines;
                let s = if n == 1 { "" } else { "s" };
                add(&mut lines, "Subtotal", format!("{} ({n} line item{s})", money_str(t.subtotal)));
                add(&mut lines, "Discount", t.discount_label.clone());
                if t.taxed_lines > 0 {
                    add(&mut lines, "Tax", format!("+{} (on {} of {n} line{s})", money_str(t.tax), t.taxed_lines));
                }
                let mut how = vec![format!("{} subtotal", money_str(t.subtotal))];
                if t.discount > 0 {
                    how.push(format!("- {} discount", money_str(t.discount)));
                }
                if t.taxed_lines > 0 {
                    how.push(format!("+ {} tax", money_str(t.tax)));
                }
                let how = if how.len() > 1 { format!(" ({})", how.join(" ")) } else { String::new() };
                add(&mut lines, "Amount", format!("{} total{how}", money_str(t.total)));
                add(&mut lines, "Line items", describe_lines(items));
            }
        }
        if currency.is_empty() {
            currency = "USD".into();
        }
    } else {
        // A discount, tax or fee next to an amount leaves it unclear whether
        // the amount is before or after it: show the arguments, not a guess.
        let adjusted = args
            .as_object()
            .is_some_and(|o| o.iter().any(|(k, v)| is_adjustment_key(k) && !v.is_null() && v.as_str() != Some("")));
        if adjusted {
            add(&mut lines, "Amount", AMOUNT_SEE_DETAILS.into());
            add(&mut lines, "Details", clip(args.to_string(), 500));
        } else {
            add(&mut lines, "Amount", given);
        }
    }
    add(&mut lines, "Currency", currency);
    add(
        &mut lines,
        "From",
        pick(args, &["fromAccountId", "sourceAccountId", "from", "accountId", "fromAccount", "businessId", "fromWallet"]),
    );
    add(
        &mut lines,
        "To",
        pick(
            args,
            &["recipientId", "toAccountId", "destinationAccountId", "to", "recipient", "toAddress", "address", "clientId", "contractorId", "projectId"],
        ),
    );
    add(&mut lines, "Memo", pick(args, &["memo", "note", "notes", "externalMemo", "description", "purpose", "remark"]));
    add(&mut lines, "Payment method", pick(args, &["paymentMethod", "method", "network", "chain"]));
    Summary {
        title: humanize_tool(tool),
        lines,
        account: None,
        note: if kind == "banking" { MERCURY_NOTE } else { MODEL_CAN_BE_WRONG }.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn held(plugin: &str, tool: &str, args: Value, summary: Option<Value>) -> Held {
        let mut e = json!({"type": "approval_needed", "plugin": plugin, "tool": tool, "arguments": args, "approval": "tok"});
        if let Some(s) = summary {
            e["summary"] = s;
        }
        Held::from_event(&e).unwrap()
    }

    #[test]
    fn embedded_allowlists_load() {
        let p = policies();
        assert_eq!(p.len(), 3);
        assert!(policy_for_slug("coinpay").unwrap().read_only.contains("invoice_list"));
        assert!(policy_for_slug("coinpay").unwrap().needs_confirmation("invoice_create_draft"));
        // Unknown names are never read-only.
        assert!(policy_for_slug("mercury").unwrap().needs_confirmation("brandNewTool"));
        assert!(!policy_for_slug("mercury").unwrap().needs_confirmation("getAccounts"));
    }

    #[test]
    fn policy_is_chosen_by_host() {
        assert_eq!(policy_for_url("https://mcp.mercury.com/mcp").unwrap().slug, "mercury");
        assert_eq!(policy_for_url("https://www.coinpayportal.com/api/mcp").unwrap().slug, "coinpay");
        assert_eq!(policy_for_url("https://MCP.moomoo.com./mcp?x=1").unwrap().slug, "moomoo");
        assert!(policy_for_url("https://mcp.mercury.com.evil.dev/mcp").is_none());
        assert!(policy_for_url("https://evil.dev/mcp.mercury.com").is_none());
        assert!(policy_for_url("not a url").is_none());
    }

    #[test]
    fn paper_mode_hides_live_order_tools() {
        let m = policy_for_slug("moomoo").unwrap();
        assert!(m.visible_in_mode("sim_trade_input_order", "paper"));
        assert!(m.visible_in_mode("quote_stock_quote", "paper"));
        assert!(!m.visible_in_mode("place_order", "paper"));
        assert!(m.visible_in_mode("place_order", "live"));
        assert!(policy_for_slug("mercury").unwrap().visible_in_mode("requestSendMoney", "paper"));
    }

    #[test]
    fn card_shows_payment_fields() {
        let mut h = held(
            "mercury",
            "requestSendMoney",
            json!({"amount": 1250.5, "accountId": "acc_1", "recipientId": "rcp_9", "paymentMethod": "ach", "externalMemo": "Nov rent", "currency": "USD"}),
            None,
        );
        h.classify(Some("https://mcp.mercury.com/mcp"));
        assert!(h.money);
        let card = render_card(&h);
        for want in ["Request send money", "Amount", "1250.5", "Currency", "USD", "From", "acc_1", "To", "rcp_9", "Memo", "Nov rent", "Payment method", "ach", "approver in Mercury"] {
            assert!(card.contains(want), "card lacks {want:?}:\n{card}");
        }
    }

    #[test]
    fn card_shows_coinpay_draft_invoice_line_items() {
        let mut h = held(
            "coinpay",
            "invoice_create_draft",
            json!({"clientId": "c_1", "lineItems": [{"quantity": 2, "unit_price": 1500}, {"quantity": 1, "unit_price": "250.25"}], "notes": "Q4"}),
            None,
        );
        h.classify(Some("https://coinpayportal.com/api/mcp"));
        let card = render_card(&h);
        assert!(card.contains("Invoice create draft"), "{card}");
        assert!(card.contains("3,250.25 total") && card.contains("Subtotal") && card.contains("2 x 1500; 1 x 250.25"), "{card}");
        assert!(card.contains("USD") && card.contains("c_1") && card.contains("Q4"), "{card}");
    }

    #[test]
    fn card_shows_order_fields_paper_and_live() {
        let args = json!({"code": "US.AAPL", "trd_side": "BUY", "qty": 10, "order_type": "NORMAL", "price": 190.5, "aux_price": 185, "acc_id": "123"});
        let mut live = held("moomoo", "place_order", args.clone(), None);
        live.classify(Some("https://mcp.moomoo.com/mcp"));
        let card = render_card(&live);
        for want in ["Place order", "[LIVE: real money]", "Symbol", "US.AAPL", "Side", "BUY", "Quantity", "10", "Order type", "NORMAL", "Limit price", "190.5", "Stop / trigger price", "185", "Estimated cost", "1,905 (quantity x price", "Account id", "123", "LIVE (real money)"] {
            assert!(card.contains(want), "card lacks {want:?}:\n{card}");
        }
        let mut paper = held("moomoo", "sim_trade_input_order", json!({"code": "US.TSLA", "qty": 1, "order_type": "MARKET"}), None);
        paper.classify(Some("https://mcp.moomoo.com/mcp"));
        let card = render_card(&paper);
        assert!(card.contains("PAPER (simulated, no real money)") && card.contains("[PAPER: simulated]"), "{card}");
        assert!(card.contains("at the market price, not known until it fills"), "{card}");
    }

    fn coinpay_card(args: Value) -> Summary {
        describe_action(policy_for_slug("coinpay").unwrap(), "invoice_create_draft", &args)
    }

    fn line<'a>(s: &'a Summary, label: &str) -> Option<&'a str> {
        s.lines.iter().find(|(l, _)| l == label).map(|(_, v)| v.as_str())
    }

    #[test]
    fn percent_discount_is_never_the_amount() {
        let s = coinpay_card(json!({"clientId": "c1", "lineItems": [{"description": "Consulting", "quantity": 2, "unit_price": 150}], "discount": {"type": "percent", "value": 10}}));
        assert_eq!(line(&s, "Subtotal"), Some("300 (1 line item)"));
        assert_eq!(line(&s, "Discount"), Some("10% = -30"));
        assert_eq!(line(&s, "Amount"), Some("270 total (300 subtotal - 30 discount)"));
        assert_eq!(line(&s, "Line items"), Some("2 x 150 Consulting"));
        assert_eq!(line(&s, "Tax"), None);
    }

    #[test]
    fn fixed_discount() {
        let s = coinpay_card(json!({"lineItems": [{"description": "a", "quantity": 1, "unit_price": 100}, {"description": "b", "quantity": 3, "unit_price": "50.5"}], "discount": {"type": "fixed", "value": 25}}));
        assert_eq!(line(&s, "Subtotal"), Some("251.5 (2 line items)"));
        assert_eq!(line(&s, "Discount"), Some("fixed -25"));
        assert_eq!(line(&s, "Amount"), Some("226.5 total (251.5 subtotal - 25 discount)"));
    }

    #[test]
    fn tax_on_the_discounted_share_like_coinpay() {
        let s = coinpay_card(json!({
            "lineItems": [{"description": "taxed", "quantity": 2, "unit_price": 150, "tax_rate": 8}, {"description": "exempt", "quantity": 1, "unit_price": 100}],
            "discount": {"type": "percent", "value": 10}
        }));
        assert_eq!(line(&s, "Subtotal"), Some("400 (2 line items)"));
        assert_eq!(line(&s, "Discount"), Some("10% = -40"));
        assert_eq!(line(&s, "Tax"), Some("+21.6 (on 1 of 2 lines)"));
        assert_eq!(line(&s, "Amount"), Some("381.6 total (400 subtotal - 40 discount + 21.6 tax)"));
        assert_eq!(invoice_totals(&json!([{"quantity": 1, "unit_price": 10, "tax_rate": 5}]), None).unwrap().total, 1050);
    }

    #[test]
    fn no_line_items_plain_amount_or_see_details() {
        let card = |tool: &str, a: Value| describe_action(policy_for_slug("coinpay").unwrap(), tool, &a);
        assert_eq!(line(&card("payment_create", json!({"amount": 42, "currency": "USDC"})), "Amount"), Some("42"));
        let adj = card("payment_create", json!({"amount": 42, "discount": {"type": "fixed", "value": 2}}));
        assert_eq!(line(&adj, "Amount"), Some("see details"));
        assert!(line(&adj, "Details").unwrap().contains("\"discount\""));
        assert_eq!(line(&card("thing_create", json!({"discount": {"value": 10}})), "Amount"), Some("see details"));
        assert_eq!(line(&card("thing_create", json!({"order": {"amount": 7}})), "Amount"), Some("7"));
        assert_eq!(line(&card("invoice_send", json!({"invoiceId": "inv_1"})), "Amount"), None);
    }

    #[test]
    fn ambiguous_line_items_show_raw_items_not_a_number() {
        for args in [
            json!({"lineItems": [{"quantity": "two", "unit_price": 150}]}),
            json!({"lineItems": [{"quantity": 1}]}),
            json!({"lineItems": [{"quantity": 1, "unit_price": 10}], "discount": {"value": 10}}),
            json!({"lineItems": [{"quantity": 1, "unit_price": 10}], "discount": {"type": "fixed", "value": 50}}),
            json!({"lineItems": [{"quantity": 1, "unit_price": 10}], "amount": 99}),
            json!({"lineItems": []}),
        ] {
            let s = coinpay_card(args.clone());
            assert!(line(&s, "Amount").unwrap().starts_with("see details"), "{args}: {s:?}");
            assert_eq!(line(&s, "Line items"), Some(args["lineItems"].to_string().as_str()));
        }
        assert_eq!(line(&coinpay_card(json!({"lineItems": [{"quantity": 1, "unit_price": 10}], "amount": 10})), "Amount"), Some("10 total"));
    }

    #[test]
    fn server_summary_wins_and_marks_money() {
        let h = held(
            "my-bank",
            "pay",
            json!({}),
            Some(json!({"title": "Send payment", "lines": [{"label": "Amount", "value": "5"}, {"label": "To", "value": "bob"}], "note": "server note"})),
        );
        assert!(h.money);
        let card = render_card(&h);
        assert!(card.contains("Send payment") && card.contains("Amount  5") && card.contains("To      bob") && card.contains("server note"), "{card}");
    }

    #[test]
    fn yes_never_approves_money() {
        let mut money = held("coinpay", "invoice_create_draft", json!({}), None);
        money.classify(Some("https://coinpayportal.com/api/mcp"));
        for yes in [false, true] {
            assert_eq!(plan(&money, yes, false), Plan::Decline(MONEY_NEEDS_TERMINAL));
            assert_eq!(plan(&money, yes, true), Plan::Ask);
        }
        // A custom slug on a money host is still money.
        let mut renamed = held("bank2", "requestSendMoney", json!({}), None);
        assert!(!renamed.money);
        renamed.classify(Some("https://mcp.mercury.com/mcp"));
        assert!(renamed.money);
        assert_eq!(plan(&renamed, true, false), Plan::Decline(MONEY_NEEDS_TERMINAL));
        // A failed URL lookup is treated as money.
        let mut unknown = held("mystery", "do_it", json!({}), None);
        unknown.classify(None);
        assert_eq!(plan(&unknown, true, false), Plan::Decline(MONEY_NEEDS_TERMINAL));
    }

    #[test]
    fn yes_approves_other_held_calls() {
        let mut h = held("docs", "write_page", json!({"id": 1}), None);
        h.classify(Some("https://docs.example.com/mcp"));
        assert!(!h.money);
        assert_eq!(plan(&h, true, false), Plan::Approve);
        assert_eq!(plan(&h, true, true), Plan::Approve);
        assert_eq!(plan(&h, false, true), Plan::Ask);
        assert_eq!(plan(&h, false, false), Plan::Decline(NEEDS_TERMINAL));
    }

    #[test]
    fn only_an_explicit_y_approves() {
        for a in ["y", "Y", "yes", " YES \n"] {
            assert!(accepts(a), "{a:?}");
        }
        for a in ["", "\n", "n", "no", "yy", "sure", "1", "ok"] {
            assert!(!accepts(a), "{a:?}");
        }
    }

    #[test]
    fn numbers_format_like_en_us() {
        assert_eq!(format_num(1905.0, 4), "1,905");
        assert_eq!(format_num(1234567.891234, 4), "1,234,567.8912");
        assert_eq!(format_num(0.5, 8), "0.5");
        assert_eq!(format_num(-1000.0, 2), "-1,000");
        assert_eq!(humanize_tool("requestSendMoney"), "Request send money");
        assert_eq!(humanize_tool("invoice_create_draft"), "Invoice create draft");
    }
}
