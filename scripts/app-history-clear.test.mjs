// The part of static/app.js that drops htmx's history cache on the sign-in pages (the kit's
// shared-tab privacy fix). It is the block between the history-clear markers, so the test runs
// just that, in a vm with a stand-in document and sessionStorage.

import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";
import { describe, expect, it } from "./test-support.mjs";

const source = readFileSync(new URL("../static/app.js", import.meta.url), "utf8");
const block = source.match(/\/\* history-clear:start[\s\S]*?\/\* history-clear:end \*\//);

const KEY = "htmx-history-cache";

function run({ body, storage }) {
  const window = { sessionStorage: storage };
  runInNewContext(block[0], { window, document: { body } });
}

function storageWith(entries) {
  const data = new Map(Object.entries(entries));
  return {
    data,
    getItem: (k) => (data.has(k) ? data.get(k) : null),
    removeItem: (k) => void data.delete(k),
  };
}

describe("history-clear block in static/app.js", () => {
  it("is present", () => {
    expect(block).toBeTruthy();
  });

  it("removes the htmx history cache on a page marked data-clear-history", () => {
    const storage = storageWith({ [KEY]: '[{"url":"/settings/tokens"}]', other: "kept" });
    run({ body: { hasAttribute: (name) => name === "data-clear-history" }, storage });
    expect(storage.getItem(KEY)).toBe(null);
    expect(storage.getItem("other")).toBe("kept");
  });

  it("leaves the cache alone on any other page", () => {
    const storage = storageWith({ [KEY]: "[]" });
    run({ body: { hasAttribute: () => false }, storage });
    expect(storage.getItem(KEY)).toBe("[]");
  });

  it("does not throw without a body or with blocked storage", () => {
    run({ body: null, storage: storageWith({}) });
    const blocked = {
      get removeItem() {
        throw new Error("SecurityError");
      },
    };
    run({ body: { hasAttribute: () => true }, storage: blocked });
    // Reading window.sessionStorage itself can throw (storage blocked by the browser).
    const window = {
      get sessionStorage() {
        throw new Error("SecurityError");
      },
    };
    runInNewContext(block[0], { window, document: { body: { hasAttribute: () => true } } });
  });
});
