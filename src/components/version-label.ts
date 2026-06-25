/**
 * Version label — fills every `.app-version` footer placeholder with the app
 * version. The placeholders live INSIDE the overlay card and the settings panel
 * surfaces (see usage-card.ts and settings-panel.ts), so the version sits as a
 * footer row within the overlay rather than floating outside it.
 */

import { getVersion } from '@tauri-apps/api/app';

/** Populate all `.app-version` placeholders from tauri.conf.json's version. */
export async function init(): Promise<void> {
  let text: string;
  try {
    text = `v${await getVersion()}`;
  } catch {
    return; // leave placeholders blank if the version can't be read
  }
  document
    .querySelectorAll<HTMLElement>('.app-version')
    .forEach((el) => {
      el.textContent = text;
    });
}
