import assert from "node:assert/strict";
import { test } from "node:test";
import { renameGroup } from "../src/rename.ts";

const channel = (handle: number, group: string) => ({
  handle, name: `ch${handle}`, title: "", group, url: "", epgId: undefined, logo: "", chno: undefined,
  itemType: "live" as const, cluster: "live" as const, inputName: "in", inputStreamId: `${handle}`, extra: [],
});

test("renames only the configured group", () => {
  const changes = renameGroup([channel(1, "News"), channel(2, "Sport")], { rename_from: "News", rename_to: "Plugin News" });
  assert.equal(changes.length, 1);
  const [change] = changes;
  assert.equal(change.tag, "modified");
  if (change.tag === "modified") {
    assert.equal(change.val.group, "Plugin News");
  }
});

test("an empty or unchanged rule renames nothing", () => {
  assert.deepEqual(renameGroup([channel(1, "News")], {}), []);
  assert.deepEqual(renameGroup([channel(1, "News")], { rename_from: "News", rename_to: "News" }), []);
});
