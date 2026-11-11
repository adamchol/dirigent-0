import { expect, test } from "bun:test";

import { onGithub } from "./api.ts";

test("github.com repositories are told from other URLs as control tells them", () => {
  for (const url of [
    "https://github.com/adamchol/my.repo",
    "https://github.com/adamchol/my.repo.git",
    "https://github.com/adamchol/my.repo/",
    " git@github.com:adamchol/my.repo.git",
    "ssh://git@github.com/adamchol/my.repo",
  ])
    expect(onGithub(url)).toBe(true);
  for (const url of ["https://gitlab.com/a/b", "https://github.com/adamchol", "https://github.com/a/b/tree/main", "https://github.com.evil.com/a/b", ""])
    expect(onGithub(url)).toBe(false);
});
