import type { Page } from "@playwright/test";
import path from "node:path";
import type { StoryDensity, StoryTheme, VisualStoryCase } from "./storybook-cases";

export function storyUrl(story: VisualStoryCase, theme: StoryTheme, density: StoryDensity) {
  const globals = encodeURIComponent(`theme:${theme};density:${density}`);
  return `/iframe.html?id=${story.id}&viewMode=story&globals=${globals}`;
}

export async function prepareStory(page: Page, story: VisualStoryCase, theme: StoryTheme, density: StoryDensity) {
  await page.goto(storyUrl(story, theme, density));
  await page.locator("#storybook-root, #root").first().waitFor({ state: "visible" });
  await page.addStyleTag({ path: path.join(process.cwd(), "tests", "storybook-screenshot.css") });
  await page.evaluate(
    ({ selectedTheme, selectedDensity }) => {
      document.documentElement.dataset.theme = selectedTheme;
      document.documentElement.dataset.density = selectedDensity;
    },
    { selectedTheme: theme, selectedDensity: density }
  );
  await page.evaluate(async () => {
    if ("fonts" in document) {
      await document.fonts.ready;
    }
  });
  await page.waitForTimeout(100);
}
