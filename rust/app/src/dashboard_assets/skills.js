(() => {
  "use strict";

  const { notice, select } = window.nineRouterUi;
  const output = select("skill-status");
  for (const button of document.querySelectorAll("button[data-skill-link]")) {
    button.addEventListener("click", async () => {
      try {
        await navigator.clipboard.writeText(button.dataset.skillLink);
        notice(output, "Link copied.");
      } catch {
        notice(output, "Copy the link from the skill row.", true);
      }
    });
  }
})();
