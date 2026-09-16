import { expect, test } from "@playwright/test";
import { visualModes, visualStoryCases } from "./storybook-cases";
import { prepareStory } from "./storybook-utils";

test.describe("dashboard story visuals", () => {
  for (const story of visualStoryCases) {
    for (const mode of visualModes) {
      test(`${story.label} ${mode.theme} ${mode.density}`, async ({ page }) => {
        await prepareStory(page, story, mode.theme, mode.density);
        const storyRoot = page.locator("#storybook-root, #root").first();
        await expect(storyRoot).toHaveScreenshot(`${story.label}-${mode.theme}-${mode.density}.png`, {
          animations: "disabled",
          caret: "hide",
          maxDiffPixelRatio: 0.002,
          maxDiffPixels: 96
        });
      });
    }
  }
});
