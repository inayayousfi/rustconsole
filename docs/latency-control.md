# Latency-aware bitrate control

inayayousfi directed the work and made every decision.
GPT-6.1 Sol, running in OpenCode, carried inayayousfi's decisions out.

## Ownership and compatibility

The client is the control application. It stores a shared maximum delay, initially 100 ms, and sends it when launching the player. The player receives and presents the stream. Player core calculates playback measurements and bounded reports; host core owns baselines, budgets, scoring, and bitrate decisions. Platform adapters supply native timestamps and apply encoder settings.

The network protocol is 3.0 and the client-player process protocol is 5. Older network peers are rejected during initial version negotiation. Host, player, and client must be rebuilt together.

## Measurements

Startup uses eight small request/reply exchanges, spaced 25 ms apart, before the existing bandwidth probe. Another eight exchanges follow that probe to refresh clock alignment before media starts.

During normal play, the player reports the largest measured delay in each 500 ms interval. Video measures capture to presentation, or labelled presentation submission when confirmed display feedback is unavailable. Repeated source captures do not create additional delay samples. Audio estimates capture to playback from submission time and the queued audio duration. Input measures local event to host acknowledgement, including local queueing. Pending acknowledgements report elapsed waiting time as an estimate.

Clock uncertainty accompanies media measurements. Clock estimates expire after five seconds. Missing measurements are unavailable, not zero delay; inactive audio and input do not count as failures. Input acknowledgement does not prove that the game responded, and queued audio does not prove its physical playback time.

## Baselines and budgets

Video, active audio, and active input each have a separate baseline. Network round-trip delay remains an existing delivery-control signal, not a fourth enforced budget.

Calibration uses five valid reports at the minimum video bitrate of 1 Mbit/s, after one second of settling and confirmation that the player has presented that bitrate. The second-lowest report establishes the baseline. Later minimum-bitrate measurements can lower it, but cannot raise it. A newly active channel without a baseline requires calibration at minimum bitrate before increases resume.

For each channel:

```text
budget = min(client maximum delay, baseline + max(baseline, 20 ms))
```

For a budget above baseline:

```text
pressure = max(0, observed delay - baseline) / (budget - baseline)
```

When the absolute limit leaves no allowance above baseline, pressure is observed delay divided by budget. The highest active-channel pressure governs control. Clock uncertainty remains visible but is not itself counted as observed delay.

## Bitrate preference and recovery

Within the budgets:

```text
latency preference = 1 - clamp(highest pressure, 0, 1)
quality preference = sqrt(clamp(bitrate / quality reference, 0, 1))
score = 0.6 * latency preference + 0.4 * quality preference
```

Bitrate is an imperfect substitute for image quality. Its square-root reward gives diminishing benefits for increases. The quality reference is the delivery-derived ceiling, or the configured maximum before that ceiling exists. The configured maximum remains binding.

The latency-derived ceiling stays separate from the delivery-derived ceiling. The lower ceiling guides recovery, with bounded probes permitted to test improvement. Each increase gets five seconds of score evaluation after settling. A worse score restores the preceding bitrate even if delays remain within their budgets.

Two consecutive excessive-delay reports undo a recent increase when applicable, otherwise reduce bitrate by 25%. Existing delivery and sender-pressure responses remain active. Missing measurements cannot justify increases or validate a score comparison.

## Session termination

After minimum bitrate takes effect and settles, five seconds of uninterrupted excessive-delay reports for any one active channel ends the session. A healthy or missing measurement for that channel, a report gap longer than two seconds, or a bitrate change resets its failure timer.

The host sends a structured failure. The player stops instead of automatically reconnecting, and the client displays the failing channel, measured delay, budget, absolute limit, baseline, estimate status, and unsuccessful reduction to 1 Mbit/s.

Tests cover the calculations, calibration and recovery sequences, independent ceilings, protocol fixtures, clock exchanges, and delivery of the terminal failure to player core. These checks do not establish live capture, physical playback, compositor timing, or gameplay performance.
