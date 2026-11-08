//! Outlook-style inbox rules: "When a message arrives that matches
//! <conditions>, do <actions>, except if <exceptions>."

use serde::{Deserialize, Serialize};

use crate::message::{Addr, MessageMeta};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Field {
    From,
    To,
    Cc,
    /// To or Cc.
    AnyRecipient,
    Subject,
    Body,
    SubjectOrBody,
    HasAttachment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Contains,
    NotContains,
    Is,
    StartsWith,
    EndsWith,
    /// Whole-value wildcard match: `*` = any run of characters, `?` = one.
    #[serde(alias = "regex")]
    Matches,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Condition {
    pub field: Field,
    pub op: Op,
    /// For text ops, several values separated by `,` or `;` match if any
    /// one matches (Outlook behaviour). Case-insensitive.
    #[serde(default)]
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    MoveTo { folder: String },
    CopyTo { folder: String },
    MarkRead,
    Flag,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    #[serde(default)]
    pub id: Option<i64>,
    pub name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// None = applies to every account.
    #[serde(default)]
    pub account_id: Option<i64>,
    /// true = all conditions must match; false = any.
    #[serde(default = "yes")]
    pub match_all: bool,
    pub conditions: Vec<Condition>,
    #[serde(default)]
    pub exceptions: Vec<Condition>,
    pub actions: Vec<Action>,
    #[serde(default = "yes")]
    pub stop_processing: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Outcome {
    pub move_to: Option<String>,
    pub copy_to: Vec<String>,
    pub mark_read: bool,
    pub flag: bool,
    pub delete: bool,
    pub matched_rules: Vec<String>,
}

impl Outcome {
    pub fn is_empty(&self) -> bool {
        self.move_to.is_none() && self.copy_to.is_empty() && !self.mark_read && !self.flag && !self.delete
    }
}

impl Condition {
    pub fn matches(&self, m: &MessageMeta, body: Option<&str>) -> bool {
        if self.op == Op::NotContains {
            let positive = Condition { op: Op::Contains, ..self.clone() };
            return self.field != Field::HasAttachment && !positive.matches(m, body);
        }
        match self.field {
            Field::HasAttachment => m.has_attachments,
            Field::From => self.match_addrs(std::slice::from_ref(&m.from)),
            Field::To => self.match_addrs(&m.to),
            Field::Cc => self.match_addrs(&m.cc),
            Field::AnyRecipient => self.match_addrs(&m.to) || self.match_addrs(&m.cc),
            Field::Subject => self.match_text(&m.subject),
            Field::Body => body.is_some_and(|b| self.match_text(b)),
            Field::SubjectOrBody => self.match_text(&m.subject) || body.is_some_and(|b| self.match_text(b)),
        }
    }

    fn match_addrs(&self, addrs: &[Addr]) -> bool {
        addrs.iter().any(|a| {
            self.match_text(&a.email)
                || (!a.name.is_empty() && self.match_text(&a.name))
                || (self.op == Op::Contains && self.match_text(&format!("{} <{}>", a.name, a.email)))
        })
    }

    fn match_text(&self, hay: &str) -> bool {
        if self.op == Op::Matches {
            return wildcard(&self.value.to_lowercase(), &hay.to_lowercase());
        }
        let hay = hay.to_lowercase();
        self.value
            .split([',', ';'])
            .map(|v| v.trim().to_lowercase())
            .filter(|v| !v.is_empty())
            .any(|v| match self.op {
                Op::Contains | Op::NotContains => hay.contains(&v),
                Op::Is => hay.trim() == v,
                Op::StartsWith => hay.trim_start().starts_with(&v),
                Op::EndsWith => hay.trim_end().ends_with(&v),
                Op::Matches => unreachable!(),
            })
    }
}

impl Rule {
    pub fn applies_to_account(&self, account_id: i64) -> bool {
        self.account_id.is_none_or(|a| a == account_id)
    }

    /// No conditions = applies to all messages (as in Outlook).
    pub fn matches(&self, m: &MessageMeta, body: Option<&str>) -> bool {
        if !self.enabled {
            return false;
        }
        let hit = if self.conditions.is_empty() {
            true
        } else if self.match_all {
            self.conditions.iter().all(|c| c.matches(m, body))
        } else {
            self.conditions.iter().any(|c| c.matches(m, body))
        };
        hit && !self.exceptions.iter().any(|c| c.matches(m, body))
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("Give the rule a name".into());
        }
        if self.actions.is_empty() {
            return Err("Choose at least one action".into());
        }
        for c in self.conditions.iter().chain(&self.exceptions) {
            if c.field != Field::HasAttachment && c.value.trim().is_empty() {
                return Err("Fill in a value for every condition".into());
            }
        }
        for a in &self.actions {
            if let Action::MoveTo { folder } | Action::CopyTo { folder } = a {
                if folder.trim().is_empty() {
                    return Err("Choose a folder".into());
                }
            }
        }
        Ok(())
    }

    /// "Always move messages from <sender> to <folder>".
    pub fn move_from_sender(sender: &str, folder: &str) -> Rule {
        Rule {
            id: None,
            name: format!("{sender} → {folder}"),
            enabled: true,
            account_id: None,
            match_all: true,
            conditions: vec![Condition { field: Field::From, op: Op::Contains, value: sender.to_string() }],
            exceptions: vec![],
            actions: vec![Action::MoveTo { folder: folder.to_string() }],
            stop_processing: true,
        }
    }
}

/// Linear-time wildcard match (`*`, `?`) over chars, anchored at both ends.
fn wildcard(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// Runs rules in order. The first matching move wins; later moves are
/// ignored. `stop_processing` halts evaluation after a matching rule.
pub fn evaluate(rules: &[Rule], m: &MessageMeta, body: Option<&str>) -> Outcome {
    let mut out = Outcome::default();
    for r in rules {
        if !r.matches(m, body) {
            continue;
        }
        out.matched_rules.push(r.name.clone());
        for a in &r.actions {
            match a {
                Action::MoveTo { folder } => {
                    if out.move_to.is_none() {
                        out.move_to = Some(folder.clone());
                    }
                }
                Action::CopyTo { folder } => out.copy_to.push(folder.clone()),
                Action::MarkRead => out.mark_read = true,
                Action::Flag => out.flag = true,
                Action::Delete => out.delete = true,
            }
        }
        if r.stop_processing {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg() -> MessageMeta {
        MessageMeta {
            subject: "[JIRA] Build failed on main".into(),
            from: Addr { name: "Jira Bot".into(), email: "jira@corp.example".into() },
            to: vec![Addr { name: "".into(), email: "team@corp.example".into() }],
            ..Default::default()
        }
    }

    fn cond(field: Field, op: Op, value: &str) -> Condition {
        Condition { field, op, value: value.into() }
    }

    #[test]
    fn sender_rule_moves() {
        let r = Rule::move_from_sender("jira@corp.example", "Notifications");
        let o = evaluate(&[r], &msg(), None);
        assert_eq!(o.move_to.as_deref(), Some("Notifications"));
    }

    #[test]
    fn multiple_values_any() {
        let c = cond(Field::Subject, Op::Contains, "deploy; build failed");
        assert!(c.matches(&msg(), None));
        let c = cond(Field::From, Op::Contains, "Jira Bot");
        assert!(c.matches(&msg(), None));
        let c = cond(Field::From, Op::Is, "JIRA@corp.example");
        assert!(c.matches(&msg(), None));
    }

    #[test]
    fn negation_and_exceptions() {
        assert!(!cond(Field::Subject, Op::NotContains, "build").matches(&msg(), None));
        let mut r = Rule::move_from_sender("jira", "X");
        r.exceptions.push(cond(Field::Subject, Op::StartsWith, "[jira]"));
        assert!(evaluate(&[r], &msg(), None).is_empty());
    }

    #[test]
    fn any_vs_all() {
        let mut r = Rule::move_from_sender("nobody@x", "X");
        r.conditions.push(cond(Field::AnyRecipient, Op::EndsWith, "@corp.example"));
        assert!(!r.matches(&msg(), None));
        r.match_all = false;
        assert!(r.matches(&msg(), None));
    }

    #[test]
    fn stop_processing_and_first_move_wins() {
        let mut a = Rule::move_from_sender("jira", "A");
        a.stop_processing = false;
        a.actions.push(Action::MarkRead);
        let b = Rule::move_from_sender("jira", "B");
        let c = Rule::move_from_sender("jira", "C");
        let o = evaluate(&[a, b, c], &msg(), None);
        assert_eq!(o.move_to.as_deref(), Some("A"));
        assert!(o.mark_read);
        assert_eq!(o.matched_rules.len(), 2);
    }

    #[test]
    fn body_and_wildcards() {
        let c = cond(Field::Body, Op::Matches, "*order #????*shipped*");
        assert!(c.matches(&msg(), Some("Your ORDER #1234 has shipped!")));
        assert!(!c.matches(&msg(), Some("Your order #12 shipped")));
        assert!(!c.matches(&msg(), None));
        assert!(wildcard("a*b*c", "axxbyyc") && !wildcard("a*b", "ab c") && wildcard("*", "") && wildcard("?é*", "xé"));
        // Old rules saved with "regex" still load.
        let old: Condition = serde_json::from_str(r#"{"field":"subject","op":"regex","value":"x*"}"#).unwrap();
        assert_eq!(old.op, Op::Matches);
    }

    #[test]
    fn disabled_rules_do_nothing() {
        let mut r = Rule::move_from_sender("jira", "X");
        r.enabled = false;
        assert!(evaluate(&[r], &msg(), None).is_empty());
    }

    #[test]
    fn serde_shape_is_stable() {
        let r = Rule::move_from_sender("a@b", "F");
        let j = serde_json::to_value(&r).unwrap();
        assert_eq!(j["actions"][0]["type"], "move_to");
        assert_eq!(j["conditions"][0]["field"], "from");
        let back: Rule = serde_json::from_value(j).unwrap();
        assert_eq!(back, r);
    }
}
