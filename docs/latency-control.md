# Latency-aware bitrate control

inayayousfi directed the work and made every decision.
GPT-6.1 Sol, running in OpenCode, carried inayayousfi's decisions out.

## Ownership and compatibility

The client is the control application. It stores a shared maximum delay, initially 100 ms, and sends it when launching the player. The player receives and presents the stream. Player core calculates playback measurements and bounded reports; host core owns baselines, budgets, scoring, and bitrate decisions. Platform adapters supply native timestamps and apply encoder settings.

The network protocol is 3.0 and the client-player process protocol is 5. Older network peers are rejected during initial version negotiation. Host, player, and client must be rebuilt together.

## Measurements

Startup uses eight small request/reply exchanges, spaced 25 ms apart, before the existing bandwidth probe. Another eight exchanges follow that probe to refresh clock alignment before media starts.

During normal play, the player reports the largest measured delay in each 500 ms interval. Video measures capture to presentation, or labelled presentation submission when confirmed display feedback is unavailable. Repeated source captures do not create additional delay samples. Audio estimates capture to playback from submission time and the queued audio duration. Input measures local event to host acknowledgement, including local queueing. Pending acknowledgements report elapsed waiting time as an estimate.

A capture or encoder stall on the host reaches the player as extra video delay, but a lower bitrate cannot remove it. The host therefore tracks the largest capture-to-encoded time of fresh frames in each report interval, treats the lowest such value as usual, and subtracts the excess of the current and previous intervals, whichever is larger, from the reported video delay before the bitrate control reads it.

Clock uncertainty accompanies media measurements. Clock estimates expire after five seconds. Missing measurements are unavailable, not zero delay; inactive audio and input do not count as failures. Input acknowledgement does not prove that the game responded, and queued audio does not prove its physical playback time.

## Baselines and budgets

Video, active audio, and active input each have a separate baseline. Network round-trip delay remains a delivery-control signal, not a fourth enforced budget.

Each channel keeps its last five valid reports taken after one second of settling and confirmation that the player has presented the current bitrate. The second-lowest of those five reports sets the baseline. Later reports can lower a baseline but cannot raise it. A newly active channel holds bitrate increases until it has a baseline.

For each channel:

```text
budget = min(client maximum delay, baseline + max(baseline, 20 ms))
```

For a budget above baseline:

```text
pressure = max(0, observed delay - baseline) / (budget - baseline)
```

When the absolute limit leaves no allowance above baseline, pressure is observed delay divided by budget. The highest active-channel pressure governs control. Clock uncertainty remains visible but is not itself counted as observed delay.

## Bitrate limit and recovery

A session starts at 70% of the capacity measured by the startup probe, and that measurement is the first limit. Only one controller governs rate: the QUIC transport uses a fixed window, so video is never held back by a second control loop.

Two consecutive excessive-delay reports cut bitrate by 25%, or by 4% for the first cut of an incident close to the latency limit. The first cut of an incident records the bitrate as the latency limit. Further cuts within 10 seconds belong to the same incident and leave the limit unchanged, because a stall at the source says nothing about the link. Delivery and sender-pressure responses stay active alongside delay.

Bitrate rises only while every active channel has a measurement and pressure is below 0.5. A report at or above 0.5 pauses the wait for the next rise without clearing it. A rise near a limit waits 15 seconds and adds 2%. A failed rise, or a delay cut within 60 seconds of a bitrate change, doubles that wait up to ten minutes and resets the step. A passing rise at or above the limit halves the wait and doubles the step up to 16%. The steady bitrate is 96% of the lower of the delivery and latency limits. A limit does not expire with time. Below 90% of a limit, each step is checked for 1.5 seconds. Closer to a limit, each step is checked for 5 seconds and increases 2%.

An incident at a bitrate held for at least 60 seconds still cuts bitrate, but it lowers no limit: the bitrate returns after the normal five-second wait, because a bitrate that held that long did not cause the incident.

A step that fails returns to 96% of the failed bitrate. A first failure near a known limit counts the same way, so a small disturbance costs 4% rather than 25%. A second failure soon after it is treated as real trouble and cuts harder. The wait before the next step near the limit starts at 15 seconds and doubles after each failure, up to 10 minutes. A validated step more than 5% above the limit proves the connection improved, so the delivery limit is forgotten and the wait resets.

The score combines latency and quality for display:

```text
latency preference = 1 - clamp(highest pressure, 0, 1)
quality preference = sqrt(clamp(bitrate / quality reference, 0, 1))
score = 0.6 * latency preference + 0.4 * quality preference
```

The score does not decide increases, because ordinary delay jitter is far larger than the score change from one bitrate step.

## Session termination

After minimum bitrate takes effect and settles, five seconds of uninterrupted excessive-delay reports for any one active channel ends the session. A healthy or missing measurement for that channel, a report gap longer than two seconds, or a bitrate change resets its failure timer.

The host sends a structured failure. The player stops instead of automatically reconnecting, and the client displays the failing channel, measured delay, budget, absolute limit, baseline, estimate status, and unsuccessful reduction to 1 Mbit/s.

Tests cover the calculations, recovery sequences, behavior on a simulated noisy link, protocol fixtures, clock exchanges, and delivery of the terminal failure to player core. `apps/rustconsole-lab` measures a real stream without a display. Neither establishes physical playback, compositor timing, or gameplay performance.
