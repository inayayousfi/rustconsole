(function (root) {
  class UniqueAsyncQueue {
    constructor() {
      this.generation = 0;
      this.items = [];
      this.keys = new Set();
      this.running = false;
    }

    reset(generation) {
      this.generation = generation;
      this.items = [];
      this.keys.clear();
    }

    enqueue(generation, key, value) {
      const token = `${generation}\u0000${key}`;
      if (generation !== this.generation || this.keys.has(token)) return false;
      this.keys.add(token);
      this.items.push({ generation, token, value });
      return true;
    }

    isCurrent(generation) {
      return generation === this.generation;
    }

    async drain(handle) {
      if (this.running) return;
      this.running = true;
      try {
        while (this.items.length !== 0) {
          const item = this.items.shift();
          try {
            await handle(item.value, item.generation);
          } finally {
            this.keys.delete(item.token);
          }
        }
      } finally {
        this.running = false;
      }
    }
  }

  const api = { UniqueAsyncQueue };
  root.RustConsoleDiscoveryAuth = api;
  if (typeof module !== "undefined") module.exports = api;
})(globalThis);
