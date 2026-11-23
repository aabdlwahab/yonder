// The theme toggle and the copy buttons. Everything on the page reads without it.

(function () {
  var root = document.documentElement;
  var toggle = document.getElementById("theme");
  if (toggle) {
    toggle.addEventListener("click", function () {
      var next = root.getAttribute("data-theme") === "dark" ? "light" : "dark";
      root.setAttribute("data-theme", next);
      try {
        localStorage.setItem("yonder-theme", next);
      } catch (error) {
        // Private windows can refuse storage; the toggle still works for this visit.
      }
    });
  }

  document.querySelectorAll("[data-copy]").forEach(function (button) {
    button.addEventListener("click", function () {
      var block = document.getElementById(button.getAttribute("data-copy"));
      if (!block) return;
      // The "$ " prompts are for reading, not pasting.
      var text = Array.prototype.map
        .call(block.childNodes, function (node) {
          return node.nodeType === 1 && node.classList.contains("p") ? "" : node.textContent;
        })
        .join("")
        .trim();
      var done = function (label) {
        button.textContent = label;
        setTimeout(function () {
          button.textContent = "Copy";
        }, 1600);
      };
      if (navigator.clipboard && navigator.clipboard.writeText) {
        navigator.clipboard.writeText(text).then(
          function () { done("Copied"); },
          function () { done("Select and copy"); }
        );
      } else {
        done("Select and copy");
      }
    });
  });
})();
