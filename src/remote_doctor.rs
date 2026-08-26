//! Pure parsing and classification primitives for `remote doctor`.
//!
//! The implementations are intentionally left as RED-phase stubs. The tests
//! below define the behavior PRD #345's production implementation must supply.

/// One forwarding directive from ssh's resolved (`ssh -G`) configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedForward {
    /// A SOCKS listener on the remote, emitted by ssh as `[socks]:0`.
    RemoteDynamic { listen: String },
    /// A remote listener forwarding to a concrete host and port.
    Remote { listen: String, destination: String },
    /// A SOCKS listener on the laptop (the wrong direction for PRD #97).
    Dynamic { listen: String },
    /// A laptop-side local forward.
    Local { listen: String, destination: String },
}

/// The subset of resolved client-side ssh configuration used by the doctor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedSshConfig {
    pub forwards: Vec<ResolvedForward>,
    pub exit_on_forward_failure: Option<bool>,
    pub forward_agent: Option<bool>,
}

/// Resolved values accepted by sshd for `AllowTcpForwarding`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowTcpForwarding {
    Yes,
    No,
    Local,
    Remote,
    All,
}

/// The subset of remote-side `sshd -T` output used by the doctor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedSshdConfig {
    pub allow_tcp_forwarding: Option<AllowTcpForwarding>,
    pub client_alive_interval: Option<u64>,
}

/// Stable identities for the doctor's dependency-ordered checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckId {
    HostReachable,
    RemoteBinary,
    ProtocolCompatible,
    RemoteForward,
    DynamicForward,
    ExitOnForwardFailure,
    AllowTcpForwarding,
    ClientAliveInterval,
    ForwardBound,
    ForwardAgent,
}

/// User-visible status of one doctor check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Warn,
    Fail,
    Unknown,
}

/// One user-visible doctor result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    pub check: CheckId,
    pub verdict: Verdict,
    pub headline: String,
    pub fix: String,
}

/// All observations consumed by the pure classifier.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DoctorInputs {
    pub host_reachable: Option<bool>,
    pub remote_binary_present: Option<bool>,
    pub protocol_compatible: Option<bool>,
    pub ssh: ResolvedSshConfig,
    pub sshd: ResolvedSshdConfig,
    pub forward_bound: Option<bool>,
}

/// Parse the known subset of `ssh -G` output, ignoring everything else.
pub fn parse_ssh_g(_stdout: &str) -> ResolvedSshConfig {
    ResolvedSshConfig::default()
}

/// Parse the known subset of a completed `sshd -T` invocation.
pub fn parse_sshd_t(_status: i32, _stdout: &str, _stderr: &str) -> ResolvedSshdConfig {
    ResolvedSshdConfig::default()
}

/// Classify parsed observations into dependency-ordered user-visible checks.
pub fn classify(_inputs: &DoctorInputs) -> Vec<CheckResult> {
    Vec::new()
}

/// Collapse checks without ever treating an UNKNOWN observation as PASS.
pub fn overall_verdict(_checks: &[CheckResult]) -> Verdict {
    Verdict::Pass
}

#[cfg(test)]
mod tests {
    use super::*;

    const REALISTIC_SSH_G: &str = r#"
host localhost
user vfarcic
hostname localhost
port 22
addressfamily any
batchmode no
canonicalizefallbacklocal yes
canonicalizehostname false
checkhostip no
compression no
controlmaster false
clearallforwardings no
exitonforwardfailure no
fingerprinthash SHA256
forwardx11 no
gatewayports no
gssapiauthentication yes
hashknownhosts yes
hostbasedauthentication no
identitiesonly no
kbdinteractiveauthentication yes
passwordauthentication yes
permitlocalcommand no
proxyusefdpass no
pubkeyauthentication true
requesttty auto
sessiontype default
stdinnull no
forkafterauthentication no
streamlocalbindunlink no
stricthostkeychecking ask
tcpkeepalive yes
serveralivecountmax 3
serveraliveinterval 0
identityfile ~/.ssh/id_rsa
identityfile ~/.ssh/id_ed25519
sendenv LANG
sendenv LC_*
permitremoteopen any
forwardagent no
connecttimeout none
controlpersist no
thiskeyisfromafutureopenssh frobnicate
remoteforward missing-destination
line-without-a-value
exitonforwardfailure yes
forwardagent yes
remoteforward 1080 [socks]:0
"#;

    fn healthy_inputs() -> DoctorInputs {
        DoctorInputs {
            host_reachable: Some(true),
            remote_binary_present: Some(true),
            protocol_compatible: Some(true),
            ssh: ResolvedSshConfig {
                forwards: vec![ResolvedForward::RemoteDynamic {
                    listen: "1080".to_string(),
                }],
                exit_on_forward_failure: Some(true),
                forward_agent: Some(false),
            },
            sshd: ResolvedSshdConfig {
                allow_tcp_forwarding: Some(AllowTcpForwarding::Yes),
                client_alive_interval: Some(30),
            },
            forward_bound: Some(true),
        }
    }

    fn check(results: &[CheckResult], id: CheckId) -> &CheckResult {
        results
            .iter()
            .find(|result| result.check == id)
            .unwrap_or_else(|| panic!("missing {id:?} check in {results:#?}"))
    }

    /// Scenario: Parse all forward forms emitted by `ssh -G` and keep laptop-side,
    /// concrete reverse, and reverse-dynamic SOCKS forwards distinguishable.
    #[test]
    fn ssh_g_parses_forward_directions_and_boolean_settings() {
        let parsed = parse_ssh_g(
            "remoteforward 1080 [socks]:0\n\
             remoteforward 127.0.0.1:8080 db.internal:5432\n\
             dynamicforward 9099\n\
             localforward 8080 localhost:80\n\
             exitonforwardfailure yes\n\
             forwardagent no\n",
        );

        assert_eq!(
            parsed.forwards,
            vec![
                ResolvedForward::RemoteDynamic {
                    listen: "1080".to_string(),
                },
                ResolvedForward::Remote {
                    listen: "127.0.0.1:8080".to_string(),
                    destination: "db.internal:5432".to_string(),
                },
                ResolvedForward::Dynamic {
                    listen: "9099".to_string(),
                },
                ResolvedForward::Local {
                    listen: "8080".to_string(),
                    destination: "localhost:80".to_string(),
                },
            ]
        );
        assert_eq!(parsed.exit_on_forward_failure, Some(true));
        assert_eq!(parsed.forward_agent, Some(false));
    }

    /// Scenario: Parse both boolean values for the two client settings instead
    /// of conflating an explicit `no` with an absent or unreadable setting.
    #[test]
    fn ssh_g_accepts_yes_and_no_boolean_values() {
        for (raw, expected) in [("yes", true), ("no", false)] {
            let parsed = parse_ssh_g(&format!("exitonforwardfailure {raw}\nforwardagent {raw}\n"));
            assert_eq!(
                parsed.exit_on_forward_failure,
                Some(expected),
                "ExitOnForwardFailure {raw}"
            );
            assert_eq!(parsed.forward_agent, Some(expected), "ForwardAgent {raw}");
        }
    }

    /// Scenario: Feed a captured, mostly-unrelated `ssh -G localhost` dump plus
    /// future and malformed lines; known settings after the bad lines still parse.
    #[test]
    fn ssh_g_ignores_unknown_and_malformed_lines_in_realistic_dump() {
        let parsed = parse_ssh_g(REALISTIC_SSH_G);

        assert_eq!(parsed.exit_on_forward_failure, Some(true));
        assert_eq!(parsed.forward_agent, Some(true));
        assert_eq!(
            parsed.forwards,
            vec![ResolvedForward::RemoteDynamic {
                listen: "1080".to_string(),
            }]
        );
    }

    /// Scenario: Parse a resolved config with no `RemoteForward` directives and
    /// return an empty inventory rather than rejecting otherwise-valid output.
    #[test]
    fn ssh_g_without_remote_forwards_returns_empty_inventory() {
        let parsed = parse_ssh_g("host localhost\nexitonforwardfailure yes\nforwardagent no\n");

        assert!(parsed.forwards.is_empty());
        assert_eq!(parsed.exit_on_forward_failure, Some(true));
    }

    /// Scenario: Parse every value accepted by `AllowTcpForwarding`; classify
    /// `no` and `local` as blocking a remote (`-R`) forward.
    #[test]
    fn sshd_t_parses_all_allow_tcp_forwarding_values_and_remote_blockers() {
        let cases = [
            ("yes", AllowTcpForwarding::Yes, Verdict::Pass),
            ("no", AllowTcpForwarding::No, Verdict::Fail),
            ("local", AllowTcpForwarding::Local, Verdict::Fail),
            ("remote", AllowTcpForwarding::Remote, Verdict::Pass),
            ("all", AllowTcpForwarding::All, Verdict::Pass),
        ];

        for (raw, expected, verdict) in cases {
            let sshd = parse_sshd_t(0, &format!("allowtcpforwarding {raw}\n"), "");
            assert_eq!(sshd.allow_tcp_forwarding, Some(expected), "value {raw}");

            let mut inputs = healthy_inputs();
            inputs.sshd = sshd;
            let results = classify(&inputs);
            assert_eq!(
                check(&results, CheckId::AllowTcpForwarding).verdict,
                verdict,
                "remote-forward semantics for {raw}"
            );
        }
    }

    /// Scenario: Parse zero (disabled) and a positive `ClientAliveInterval`
    /// as integers so classification can distinguish stale-listener risk.
    #[test]
    fn sshd_t_parses_client_alive_interval_zero_and_nonzero() {
        assert_eq!(
            parse_sshd_t(0, "clientaliveinterval 0\n", "").client_alive_interval,
            Some(0)
        );
        assert_eq!(
            parse_sshd_t(0, "clientaliveinterval 45\n", "").client_alive_interval,
            Some(45)
        );
    }

    /// Scenario: Simulate empty, failed, and permission-denied `sshd -T`
    /// probes; none may turn plausible-looking settings into known PASS data.
    #[test]
    fn sshd_t_unavailable_is_unknown_never_pass() {
        let unavailable = [
            parse_sshd_t(0, "", ""),
            parse_sshd_t(
                1,
                "allowtcpforwarding yes\nclientaliveinterval 30\n",
                "sshd: no hostkeys available -- exiting",
            ),
            parse_sshd_t(
                0,
                "allowtcpforwarding yes\nclientaliveinterval 30\n",
                "Permission denied",
            ),
        ];

        for sshd in unavailable {
            assert_eq!(sshd.allow_tcp_forwarding, None);
            assert_eq!(sshd.client_alive_interval, None);

            let mut inputs = healthy_inputs();
            inputs.sshd = sshd;
            let results = classify(&inputs);
            assert_eq!(
                check(&results, CheckId::AllowTcpForwarding).verdict,
                Verdict::Unknown
            );
            assert_eq!(
                check(&results, CheckId::ClientAliveInterval).verdict,
                Verdict::Unknown
            );
            assert_ne!(overall_verdict(&results), Verdict::Pass);
        }
    }

    /// Scenario: Parse a successful but partial sshd dump per setting; the
    /// present keepalive gets a real verdict while the absent forwarding key is UNKNOWN.
    #[test]
    fn sshd_t_partial_output_preserves_per_check_knowledge() {
        let sshd = parse_sshd_t(0, "clientaliveinterval 30\n", "");
        assert_eq!(sshd.allow_tcp_forwarding, None);
        assert_eq!(sshd.client_alive_interval, Some(30));

        let mut inputs = healthy_inputs();
        inputs.sshd = sshd;
        let results = classify(&inputs);
        assert_eq!(
            check(&results, CheckId::AllowTcpForwarding).verdict,
            Verdict::Unknown
        );
        assert_eq!(
            check(&results, CheckId::ClientAliveInterval).verdict,
            Verdict::Pass
        );
    }

    /// Scenario: Compare the two failures that share ssh's client error text:
    /// sshd refusing remote forwards and an allowed forward whose port is unavailable.
    #[test]
    fn classify_distinguishes_sshd_block_from_port_collision() {
        let mut blocked = healthy_inputs();
        blocked.sshd.allow_tcp_forwarding = Some(AllowTcpForwarding::No);
        blocked.forward_bound = Some(false);
        let blocked_results = classify(&blocked);
        let blocked_check = check(&blocked_results, CheckId::AllowTcpForwarding);

        let mut collision = healthy_inputs();
        collision.forward_bound = Some(false);
        let collision_results = classify(&collision);
        let collision_check = check(&collision_results, CheckId::ForwardBound);

        assert_eq!(blocked_check.verdict, Verdict::Fail);
        assert_eq!(collision_check.verdict, Verdict::Fail);
        assert!(blocked_check.fix.contains("AllowTcpForwarding"));
        assert!(collision_check.fix.to_ascii_lowercase().contains("port"));
        assert_ne!(blocked_check.headline, collision_check.headline);
        assert_ne!(blocked_check.fix, collision_check.fix);
    }

    /// Scenario: Classify both an explicit `no` and an absent
    /// `ExitOnForwardFailure`; each reports the exact setting that prevents silent failure.
    #[test]
    fn classify_reports_exit_on_forward_failure_missing_or_disabled() {
        for setting in [Some(false), None] {
            let mut inputs = healthy_inputs();
            inputs.ssh.exit_on_forward_failure = setting;
            let results = classify(&inputs);
            let result = check(&results, CheckId::ExitOnForwardFailure);

            assert!(matches!(result.verdict, Verdict::Fail | Verdict::Warn));
            assert!(
                result.fix.contains("ExitOnForwardFailure yes"),
                "specific fix missing from {result:#?}"
            );
        }
    }

    /// Scenario: Compare a laptop-side `DynamicForward` mistake with no forward
    /// at all; the user must receive different headlines and fixes.
    #[test]
    fn classify_distinguishes_wrong_direction_from_no_forward() {
        let mut wrong_direction = healthy_inputs();
        wrong_direction.ssh.forwards = vec![ResolvedForward::Dynamic {
            listen: "9099".to_string(),
        }];
        wrong_direction.forward_bound = None;
        let wrong_results = classify(&wrong_direction);
        let wrong = check(&wrong_results, CheckId::DynamicForward);

        let mut missing = healthy_inputs();
        missing.ssh.forwards.clear();
        missing.forward_bound = None;
        let missing_results = classify(&missing);
        let absent = check(&missing_results, CheckId::RemoteForward);

        assert_ne!(wrong.verdict, Verdict::Pass);
        assert_ne!(absent.verdict, Verdict::Pass);
        assert!(wrong.headline.to_ascii_lowercase().contains("direction"));
        assert_ne!(wrong.headline, absent.headline);
        assert_ne!(wrong.fix, absent.fix);
    }

    /// Scenario: A correctly configured reverse-dynamic forward fails its live
    /// remote bind probe; report the unbound forward and a port-specific fix.
    #[test]
    fn classify_reports_configured_forward_not_bound() {
        let mut inputs = healthy_inputs();
        inputs.forward_bound = Some(false);
        let results = classify(&inputs);
        let result = check(&results, CheckId::ForwardBound);

        assert_eq!(result.verdict, Verdict::Fail);
        assert!(result.headline.to_ascii_lowercase().contains("bound"));
        assert!(result.fix.to_ascii_lowercase().contains("port"));
    }

    /// Scenario: Enable agent forwarding on an otherwise healthy remote; the
    /// doctor emits a security advisory without turning it into a failure.
    #[test]
    fn classify_forward_agent_is_advisory_not_failure() {
        let mut inputs = healthy_inputs();
        inputs.ssh.forward_agent = Some(true);
        let results = classify(&inputs);
        let result = check(&results, CheckId::ForwardAgent);

        assert_eq!(result.verdict, Verdict::Warn);
        assert_ne!(result.verdict, Verdict::Fail);
        assert!(result.fix.contains("ForwardAgent no"));
    }

    /// Scenario: Classify a healthy observation set and preserve dependency
    /// order: reachability first, resolved forwards next, then remote sshd policy.
    #[test]
    fn classify_orders_checks_by_dependency() {
        let results = classify(&healthy_inputs());
        let position = |id| {
            results
                .iter()
                .position(|result| result.check == id)
                .unwrap_or_else(|| panic!("missing {id:?} in {results:#?}"))
        };

        assert!(position(CheckId::HostReachable) < position(CheckId::RemoteForward));
        assert!(position(CheckId::RemoteForward) < position(CheckId::AllowTcpForwarding));
    }

    /// Scenario: Leave one remote-side observation unavailable while all other
    /// checks are healthy; aggregate status remains UNKNOWN rather than all-clear PASS.
    #[test]
    fn overall_verdict_does_not_treat_unknown_as_pass() {
        let mut inputs = healthy_inputs();
        inputs.sshd.allow_tcp_forwarding = None;
        let results = classify(&inputs);

        assert_eq!(
            check(&results, CheckId::AllowTcpForwarding).verdict,
            Verdict::Unknown
        );
        assert_eq!(overall_verdict(&results), Verdict::Unknown);
    }
}
