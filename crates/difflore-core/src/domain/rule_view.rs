//! Common view over rule-bearing records used by helpers in `origins`.

pub trait RuleView {
    fn id(&self) -> &str;
    fn content(&self) -> &str;
    fn origin(&self) -> &str;
    fn confidence(&self) -> Option<f64>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        id: String,
        content: String,
        origin: String,
        conf: Option<f64>,
    }

    impl RuleView for Fake {
        fn id(&self) -> &str {
            &self.id
        }
        fn content(&self) -> &str {
            &self.content
        }
        fn origin(&self) -> &str {
            &self.origin
        }
        fn confidence(&self) -> Option<f64> {
            self.conf
        }
    }

    #[test]
    fn trait_basic_dispatch() {
        let r = Fake {
            id: "a".into(),
            content: "c".into(),
            origin: "manual".into(),
            conf: Some(0.7),
        };
        assert_eq!(r.id(), "a");
        assert_eq!(r.content(), "c");
        assert_eq!(r.origin(), "manual");
        assert_eq!(r.confidence(), Some(0.7));
    }
}
