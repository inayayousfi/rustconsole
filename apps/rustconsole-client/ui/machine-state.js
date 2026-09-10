(function (root) {
  function playerActive(session) {
    return session.phase !== "idle";
  }

  function playerTargets(session, machine) {
    return playerActive(session) && machine.endpoints.includes(session.endpoint);
  }

  function presentation(machine, session) {
    if (playerTargets(session, machine)) {
      if (session.phase === "launching") return { state: "checking", detail: "Launching player" };
      if (session.phase === "authenticated") return { state: "connected", detail: "Authenticated" };
      if (session.phase === "stopping") return { state: "checking", detail: "Disconnecting" };
      return { state: "connected", detail: "Player streaming" };
    }
    return { state: machine.availability, detail: machine.availabilityDetail };
  }

  function action(machine, session) {
    if (playerTargets(session, machine)) {
      return { label: session.phase === "stopping" ? "Disconnecting" : "Connected", disabled: true };
    }
    const blocked = playerActive(session);
    if (machine.availability === "available") return { kind: "connect", label: "Connect", disabled: blocked };
    if (machine.availability === "authentication") return { kind: "authenticate", label: "Authenticate", disabled: blocked };
    if (machine.availability === "checking") return { label: "Checking", disabled: true };
    return { kind: "probe", label: machine.availability === "busy" ? "Check again" : "Retry", disabled: blocked };
  }

  const api = { action, playerActive, playerTargets, presentation };
  root.RustConsoleMachineState = api;
  if (typeof module !== "undefined") module.exports = api;
})(globalThis);
