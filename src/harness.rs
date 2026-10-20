// The agent CLIs drover can start. Adding one is an entry here, plus its usage script in herdr.rs.

pub struct Harness {
    pub name: &'static str,
    // the reply is read from a transcript; otherwise the terminal screen is the reply
    pub transcript: bool,
    // the prompt that quits the agent and leaves its pane at the shell
    pub exit: Option<&'static str>,
}

pub const HARNESSES: [Harness; 2] = [
    Harness {
        name: "claude",
        transcript: true,
        exit: Some("/exit"),
    },
    Harness {
        name: "cursor",
        transcript: false,
        exit: None,
    },
];

pub const DEFAULT: &str = "claude";

#[must_use]
pub fn find(name: &str) -> Option<&'static Harness> {
    HARNESSES.iter().find(|h| h.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_a_known_harness() {
        assert!(find(DEFAULT).is_some_and(|h| h.transcript));
    }

    #[test]
    fn unknown_names_are_not_harnesses() {
        assert!(find("nope").is_none());
        assert!(find("cursor").is_some_and(|h| !h.transcript));
    }
}
