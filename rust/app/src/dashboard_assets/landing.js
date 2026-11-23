const command = document.getElementById("npm-command");
const copyStatus = document.getElementById("copy-status");

document.getElementById("copy-command").addEventListener("click", async () => {
  try {
    await navigator.clipboard.writeText(command.textContent);
    copyStatus.textContent = "Copied.";
  } catch {
    const selection = window.getSelection();
    if (selection) {
      const range = document.createRange();
      range.selectNodeContents(command);
      selection.removeAllRanges();
      selection.addRange(range);
    }
    copyStatus.textContent = "Clipboard unavailable. Copy the selected command manually.";
  }
});
