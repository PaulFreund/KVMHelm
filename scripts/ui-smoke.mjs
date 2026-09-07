import { chromium } from "@playwright/test";
import { readFile, mkdir } from "node:fs/promises";
const browser = await chromium.launch({ headless: true });
const page = await browser.newPage({
  viewport: { width: 1500, height: 1000 },
  deviceScaleFactor: 1,
});
const errors = [];
page.on("pageerror", (e) => errors.push(e.message));
try {
  await page.goto("http://127.0.0.1:8765");
  await page
    .getByLabel("Personal Access Token")
    .fill((await readFile(".kvmhelm/secrets/cli.pat", "utf8")).trim());
  await page.getByRole("button", { name: "Anmelden →" }).click();
  await page.getByRole("heading", { name: "Deine Computer" }).waitFor();
  await page.locator(".display img").waitFor();
  await page
    .getByRole("button", { name: "Steuerung anfordern", exact: true })
    .click();
  await page
    .getByRole("button", { name: "Steuerung freigeben", exact: true })
    .waitFor();
  await page.locator(".display img").click({ position: { x: 200, y: 100 } });
  await page.waitForTimeout(700);
  await mkdir(".kvmhelm/qa", { recursive: true });
  await page.screenshot({ path: ".kvmhelm/qa/computer.png", fullPage: true });
  await page
    .getByRole("button", { name: "Videowand", exact: false })
    .first()
    .click();
  await page.locator(".visibility-bar input").first().check();
  await page.locator(".visibility-bar input").nth(1).check();
  await page.locator(".wall img").first().waitFor();
  await page.locator(".wall img").nth(1).waitFor();
  await page.screenshot({ path: ".kvmhelm/qa/wall.png", fullPage: true });
  await page.reload();
  await page.locator(".wall img").first().waitFor();
  if ((await page.locator(".visibility-bar input:checked").count()) !== 2)
    throw Error("Videowall selection did not persist");
  if (errors.length) throw Error(errors.join("\n"));
  console.log(
    "UI smoke passed: login, image, control, click, wall, persisted selection; zero page errors.",
  );
} catch (error) {
  console.error("Browser errors:", errors);
  console.error("Location:", page.url());
  console.error((await page.locator("body").innerText()).slice(0, 1600));
  await page.screenshot({ path: ".kvmhelm/qa/failure.png", fullPage: true });
  throw error;
} finally {
  await browser.close();
}
