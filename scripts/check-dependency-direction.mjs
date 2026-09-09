import { execFileSync } from "node:child_process";

const allowedDependencies = new Map([
  ["rustconsole-codec-ffmpeg", []],
  ["rustconsole-protocol", []],
  ["rustconsole-session", ["rustconsole-protocol"]],
  ["rustconsole-discovery", []],
  ["rustconsole-media", []],
  ["rustconsole-render", []],
  ["rustconsole-render-vulkan", ["rustconsole-render"]],
  [
    "rustconsole-render-vulkan-linux",
    [
      "rustconsole-codec-ffmpeg",
      "rustconsole-render-vulkan",
    ],
  ],
  [
    "rustconsole-host-core",
    ["rustconsole-media", "rustconsole-protocol", "rustconsole-session"],
  ],
  [
    "rustconsole-host-windows",
    [
      "rustconsole-host-core",
      "rustconsole-codec-ffmpeg",
      "rustconsole-input-windows",
      "rustconsole-media",
      "rustconsole-protocol",
      "rustconsole-session",
    ],
  ],
  [
    "rustconsole-input-windows",
    ["rustconsole-protocol", "rustconsole-session"],
  ],
  [
    "rustconsole-client-core",
    ["rustconsole-discovery", "rustconsole-player-core"],
  ],
  [
    "rustconsole-player-core",
    [
      "rustconsole-discovery",
      "rustconsole-media",
      "rustconsole-protocol",
      "rustconsole-session",
    ],
  ],
  [
    "rustconsole-player-linux",
    [
      "rustconsole-codec-ffmpeg",
      "rustconsole-media",
      "rustconsole-protocol",
      "rustconsole-player-core",
    ],
  ],
  ["rustconsole-host", ["rustconsole-host-windows"]],
  ["rustconsole-client", ["rustconsole-client-core"]],
  ["rustconsole-test-game", []],
  [
    "rustconsole-player",
    [
      "rustconsole-player-core",
      "rustconsole-player-linux",
      "rustconsole-protocol",
      "rustconsole-render",
      "rustconsole-render-vulkan",
      "rustconsole-render-vulkan-linux",
    ],
  ],
]);

const metadata = JSON.parse(
  execFileSync(
    "cargo",
    ["metadata", "--no-deps", "--format-version", "1"],
    { encoding: "utf8" },
  ),
);

const errors = [];

for (const package_ of metadata.packages) {
  const expected = allowedDependencies.get(package_.name);
  if (expected === undefined) {
    errors.push(`workspace package is missing from policy: ${package_.name}`);
    continue;
  }

  const actual = package_.dependencies
    .filter((dependency) => allowedDependencies.has(dependency.name))
    .map((dependency) => dependency.name)
    .sort();
  const sortedExpected = [...expected].sort();

  if (actual.join("\n") !== sortedExpected.join("\n")) {
    errors.push(
      `${package_.name}: expected [${sortedExpected.join(", ")}], found [${actual.join(", ")}]`,
    );
  }
}

for (const packageName of allowedDependencies.keys()) {
  if (!metadata.packages.some((package_) => package_.name === packageName)) {
    errors.push(`policy package is missing from workspace: ${packageName}`);
  }
}

if (errors.length > 0) {
  console.error("Workspace dependency policy failed:");
  for (const error of errors) {
    console.error(`- ${error}`);
  }
  process.exitCode = 1;
} else {
  console.log("Workspace dependency policy passed.");
}
