//! 余额 / 额度用量投影：逐行移植 desktop main/billing/projection.ts 的 projectModelAccessBalance / projectModelAccessCreditUsage。
//! 校验失败一律返回 Err(())（上游抛 `invalid billing response`），调用方统一映射成 INTERNAL。
//! 黄金样例：scripts/app-settings/billing-entry.ts → billing_golden.json。

use serde_json::{json, Map, Value};

const MAX_ID_LENGTH: usize = 128;
const MAX_NAME_LENGTH: usize = 128;
const MAX_PROMOTIONAL_GRANTS: usize = 1_000;
const MAX_LEDGER_SCALED_AMOUNT: i128 = 9_000_000_000_000_000_000;
const NANO: i128 = 1_000_000_000;

type R<T> = Result<T, ()>;

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

fn bounded_string(v: Option<&Value>, max: usize) -> Option<&str> {
    let s = v?.as_str()?;
    let n = utf16_len(s);
    (n > 0 && n <= max).then_some(s)
}

fn all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// `^-?(?:0|[1-9]\d{0,9})(?:\.\d{1,9})?$`，换算成 1e-9 单位；超上限返回 None。
fn ledger_decimal(v: Option<&Value>) -> Option<(String, i128)> {
    let s = v?.as_str()?;
    let (negative, unsigned) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let (whole, frac) = match unsigned.split_once('.') {
        Some((w, f)) => (w, Some(f)),
        None => (unsigned, None),
    };
    if !all_digits(whole) || whole.len() > 10 || (whole.len() > 1 && whole.starts_with('0')) {
        return None;
    }
    if let Some(f) = frac {
        if !all_digits(f) || f.len() > 9 {
            return None;
        }
    }
    let frac = frac.unwrap_or("");
    let scaled = whole.parse::<i128>().ok()? * NANO + format!("{frac:0<9}").parse::<i128>().ok()?;
    if scaled > MAX_LEDGER_SCALED_AMOUNT {
        return None;
    }
    Some((s.to_string(), if negative { -scaled } else { scaled }))
}

/// RFC3339 UTC：`YYYY-MM-DDTHH:MM:SS(.f{1,9})?Z`，并核对月日时分秒范围（秒最大 59，不接受闰秒）。
fn observed_at(v: Option<&Value>) -> Option<&str> {
    let s = v?.as_str()?;
    let b = s.as_bytes();
    if b.len() < 20 || *b.last()? != b'Z' {
        return None;
    }
    let digits = |r: std::ops::Range<usize>| b.get(r).filter(|x| x.iter().all(u8::is_ascii_digit));
    let num = |r: std::ops::Range<usize>| -> Option<u32> { std::str::from_utf8(digits(r)?).ok()?.parse().ok() };
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let (year, month, day, hour, minute, second) = (num(0..4)?, num(5..7)?, num(8..10)?, num(11..13)?, num(14..16)?, num(17..19)?);
    match &b[19..b.len() - 1] {
        [] => {}
        [b'.', rest @ ..] if (1..=9).contains(&rest.len()) && rest.iter().all(u8::is_ascii_digit) => {}
        _ => return None,
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    if !(1..=12).contains(&month) || day < 1 || day > days[(month - 1) as usize] || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    Some(s)
}

fn compare_observed_at(left: &str, right: &str) -> std::cmp::Ordering {
    let split = |s: &str| {
        let body = &s[..s.len() - 1];
        match body.split_once('.') {
            Some((w, f)) => (w.to_string(), format!("{f:0<9}")),
            None => (body.to_string(), "000000000".to_string()),
        }
    };
    split(left).cmp(&split(right))
}

pub fn project_balance(value: &Value) -> R<Value> {
    let o = value.as_object().ok_or(())?;
    if o.get("scale").and_then(Value::as_f64) != Some(9.0) {
        return Err(());
    }
    let plan = ledger_decimal(o.get("planCredits")).ok_or(())?;
    let purchased = ledger_decimal(o.get("purchasedCredits")).ok_or(())?;
    let promo = ledger_decimal(o.get("promotionalCredits")).ok_or(())?;
    let available = ledger_decimal(o.get("available")).ok_or(())?;
    let at = observed_at(o.get("observedAt")).ok_or(())?;
    if plan.1 < 0 || promo.1 < 0 || plan.1 + purchased.1 + promo.1 != available.1 {
        return Err(());
    }
    Ok(json!({
        "planCredits": plan.0, "purchasedCredits": purchased.0, "promotionalCredits": promo.0,
        "available": available.0, "scale": 9, "observedAt": at,
    }))
}

struct Pool {
    value: Value,
    remaining: i128,
    used: Option<i128>,
    total: Option<i128>,
}

fn project_pool(v: Option<&Value>, allow_negative_remaining: bool) -> R<Pool> {
    let o = v.and_then(Value::as_object).ok_or(())?;
    let remaining = ledger_decimal(o.get("remaining")).ok_or(())?;
    let nullable = |k: &str| -> R<Option<(String, i128)>> {
        match o.get(k) {
            Some(Value::Null) => Ok(None),
            other => ledger_decimal(other).map(Some).ok_or(()),
        }
    };
    let used = nullable("used")?;
    let total = nullable("total")?;
    if (!allow_negative_remaining && remaining.1 < 0) || used.is_some() != total.is_some() {
        return Err(());
    }
    if let (Some(u), Some(t)) = (&used, &total) {
        if u.1 < 0 || t.1 < 0 || u.1 + remaining.1 != t.1 {
            return Err(());
        }
    }
    Ok(Pool {
        value: json!({ "remaining": remaining.0, "used": used.as_ref().map(|x| x.0.clone()), "total": total.as_ref().map(|x| x.0.clone()) }),
        remaining: remaining.1,
        used: used.map(|x| x.1),
        total: total.map(|x| x.1),
    })
}

struct Grant {
    value: Value,
    id: String,
    state: String,
    expires_at: String,
    original: i128,
    used: i128,
    remaining: i128,
}

fn project_grant(v: &Value) -> R<Grant> {
    let o = v.as_object().ok_or(())?;
    let id = bounded_string(o.get("grantId"), MAX_ID_LENGTH).ok_or(())?;
    let display_name = match o.get("displayName") {
        Some(Value::Null) => Value::Null,
        other => Value::String(bounded_string(other, MAX_NAME_LENGTH).ok_or(())?.to_string()),
    };
    let original = ledger_decimal(o.get("originalAmount")).ok_or(())?;
    let used = ledger_decimal(o.get("usedAmount")).ok_or(())?;
    let remaining = ledger_decimal(o.get("remainingAmount")).ok_or(())?;
    let expires_at = observed_at(o.get("expiresAt")).ok_or(())?;
    let state = o.get("state").and_then(Value::as_str).filter(|s| matches!(*s, "active" | "depleted" | "expired" | "voided")).ok_or(())?;
    let (orig, u, rem) = (original.1, used.1, remaining.1);
    if orig <= 0 || u < 0 || rem < 0 {
        return Err(());
    }
    let bad = match state {
        "active" => rem == 0 || u + rem != orig,
        "depleted" => rem != 0 || u != orig,
        _ => rem != 0 || u > orig,
    };
    if bad {
        return Err(());
    }
    Ok(Grant {
        value: json!({
            "grantId": id, "displayName": display_name, "originalAmount": original.0, "usedAmount": used.0,
            "remainingAmount": remaining.0, "expiresAt": expires_at, "state": state,
        }),
        id: id.to_string(),
        state: state.to_string(),
        expires_at: expires_at.to_string(),
        original: orig,
        used: u,
        remaining: rem,
    })
}

pub fn project_credit_usage(value: &Value) -> R<Value> {
    let o = value.as_object().ok_or(())?;
    let grants_in = o.get("promotionalGrants").and_then(Value::as_array).ok_or(())?;
    let complete = o.get("promotionalGrantsComplete").and_then(Value::as_bool).ok_or(())?;
    if o.get("scale").and_then(Value::as_f64) != Some(9.0)
        || grants_in.len() > MAX_PROMOTIONAL_GRANTS
        || o.get("promotionalGrantConsistency").and_then(Value::as_str) != Some("OBSERVED")
    {
        return Err(());
    }
    let available = ledger_decimal(o.get("available"));
    let plan = project_pool(o.get("plan"), false)?;
    let purchased = project_pool(o.get("purchased"), true)?;
    let promotional = project_pool(o.get("promotional"), false)?;
    let snapshot = observed_at(o.get("observedAt"));
    let ledger_updated_at = match o.get("ledgerUpdatedAt") {
        Some(Value::Null) => Some(None),
        other => observed_at(other).map(Some),
    };
    let (Some(available), Some(snapshot), Some(ledger_updated_at)) = (available, snapshot, ledger_updated_at) else { return Err(()) };
    if plan.remaining + purchased.remaining + promotional.remaining != available.1 {
        return Err(());
    }

    let mut ids = std::collections::HashSet::new();
    let (mut cur_original, mut cur_used, mut cur_remaining) = (0i128, 0i128, 0i128);
    let mut grants = Vec::with_capacity(grants_in.len());
    for g in grants_in {
        let p = project_grant(g)?;
        if !ids.insert(p.id.clone()) {
            return Err(());
        }
        if (p.state == "active" || p.state == "depleted") && compare_observed_at(&p.expires_at, snapshot).is_gt() {
            cur_original += p.original;
            cur_used += p.used;
            cur_remaining += p.remaining;
        }
        grants.push(p.value);
    }
    let consistent = if complete {
        matches!((promotional.total, promotional.used), (Some(t), Some(u)) if t == cur_original && u == cur_used)
            && promotional.remaining == cur_remaining
    } else {
        promotional.used.is_none() && promotional.total.is_none()
    };
    if !consistent {
        return Err(());
    }
    let mut out = Map::new();
    out.insert("available".into(), json!(available.0));
    out.insert("plan".into(), plan.value);
    out.insert("purchased".into(), purchased.value);
    out.insert("promotional".into(), promotional.value);
    out.insert("promotionalGrants".into(), Value::Array(grants));
    out.insert("promotionalGrantsComplete".into(), json!(complete));
    out.insert("promotionalGrantConsistency".into(), json!("OBSERVED"));
    out.insert("ledgerUpdatedAt".into(), json!(ledger_updated_at));
    out.insert("scale".into(), json!(9));
    out.insert("observedAt".into(), json!(snapshot));
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn golden() -> Value {
        serde_json::from_str(include_str!("billing_golden.json")).unwrap()
    }

    fn replay(key: &str, f: fn(&Value) -> R<Value>, min: usize) {
        let g = golden();
        let cases = g[key].as_array().unwrap();
        assert!(cases.len() >= min, "{key}: {}", cases.len());
        for c in cases {
            let got = f(&c["input"]);
            if c["ok"].as_bool().unwrap() {
                assert_eq!(got.as_ref().ok(), Some(&c["output"]), "{}", c["input"]);
            } else {
                assert!(got.is_err(), "应拒绝: {}", c["input"]);
            }
        }
    }

    #[test]
    fn balance_matches_upstream() {
        replay("balanceCases", project_balance, 30);
    }

    #[test]
    fn credit_usage_matches_upstream() {
        replay("usageCases", project_credit_usage, 40);
    }
}
