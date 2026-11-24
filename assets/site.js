// Everything that moves on the page. Without this file every section still
// reads in order; with reduced motion, things change state without animating.

(function () {
  var root = document.documentElement;
  var reduce = window.matchMedia && matchMedia("(prefers-reduced-motion: reduce)").matches;
  var clamp = function (value, low, high) { return Math.max(low, Math.min(high, value)); };

  /* ---------- theme ---------- */
  var themeButton = document.getElementById("theme");
  if (themeButton) {
    themeButton.addEventListener("click", function () {
      var next = root.getAttribute("data-theme") === "dark" ? "light" : "dark";
      root.setAttribute("data-theme", next);
      try { localStorage.setItem("yonder-theme", next); } catch (error) { /* private windows */ }
    });
  }

  /* ---------- copy buttons ---------- */
  document.querySelectorAll("[data-copy]").forEach(function (button) {
    button.addEventListener("click", function () {
      var block = document.getElementById(button.getAttribute("data-copy"));
      if (!block) return;
      // The "$ " prompts are for reading, not for pasting.
      var text = Array.prototype.map.call(block.childNodes, function (node) {
        return node.nodeType === 1 && node.classList.contains("p") ? "" : node.textContent;
      }).join("").trim();
      var show = function (label) {
        button.textContent = label;
        setTimeout(function () { button.textContent = "Copy"; }, 1600);
      };
      if (navigator.clipboard && navigator.clipboard.writeText) {
        navigator.clipboard.writeText(text).then(function () { show("Copied"); }, function () { show("Select it to copy"); });
      } else {
        show("Select it to copy");
      }
    });
  });

  /* ---------- the story: one beat per quarter of its scroll ---------- */
  var story = document.getElementById("story");
  var scene = story && story.querySelector(".scene");
  var beats = story ? story.querySelectorAll(".beat") : [];
  var ticks = story ? story.querySelectorAll(".rail i") : [];
  var progressBar = document.querySelector(".progress");
  var currentBeat = 1;

  function storyProgress() {
    var rect = story.getBoundingClientRect();
    var travel = story.offsetHeight - window.innerHeight;
    return travel > 0 ? clamp(-rect.top / travel, 0, 1) : 0;
  }

  function showBeat(beat, approved) {
    if (beat !== currentBeat) {
      currentBeat = beat;
      scene.setAttribute("data-beat", String(beat));
      beats.forEach(function (element) { element.classList.toggle("on", Number(element.getAttribute("data-beat")) === beat); });
      ticks.forEach(function (tick, index) { tick.classList.toggle("on", index < beat); });
    }
    scene.classList.toggle("approved", approved);
  }

  /* ---------- thread map: drawn as it scrolls into view ---------- */
  var map = document.getElementById("map");
  var drawn = map ? map.querySelectorAll(".draw") : [];
  var pops = map ? map.querySelectorAll(".pop") : [];
  drawn.forEach(function (path) {
    try { path.style.setProperty("--len", String(Math.ceil(path.getTotalLength()))); } catch (error) { /* not rendered yet */ }
  });

  function drawMap() {
    var rect = map.getBoundingClientRect();
    var height = window.innerHeight;
    var progress = reduce ? 1 : clamp((height * 0.92 - rect.top) / (height * 0.75), 0, 1);
    drawn.forEach(function (path) {
      var start = Number(path.getAttribute("data-at"));
      path.style.setProperty("--drawn", String(clamp((progress - start) / 0.2, 0, 1)));
    });
    pops.forEach(function (element) {
      element.style.setProperty("--popped", progress >= Number(element.getAttribute("data-at")) + 0.04 ? "1" : "0");
    });
  }

  var ticking = false;
  function onScroll() {
    if (ticking) return;
    ticking = true;
    requestAnimationFrame(function () {
      ticking = false;
      var page = document.documentElement;
      var read = page.scrollHeight > window.innerHeight ? window.scrollY / (page.scrollHeight - window.innerHeight) : 0;
      if (progressBar) progressBar.style.setProperty("--read", String(clamp(read, 0, 1)));
      if (story && scene) {
        var progress = storyProgress();
        showBeat(Math.min(4, Math.floor(progress * 4) + 1), progress > 0.88);
      }
      if (map) drawMap();
      parallax();
    });
  }

  /* ---------- parallax: words, windows and the hero drift with the scroll ---------- */
  var drifting = document.querySelectorAll("[data-drift]");
  var desks = document.getElementById("desks");
  var heroSection = document.getElementById("hero");
  function parallax() {
    if (reduce) return;
    var height = window.innerHeight;
    drifting.forEach(function (element) {
      var rect = element.parentElement.getBoundingClientRect();
      if (rect.bottom < -200 || rect.top > height + 200) return;
      var offset = (rect.top + rect.height / 2 - height / 2) * Number(element.getAttribute("data-drift"));
      element.style.setProperty("--drift", offset.toFixed(1));
    });
    if (desks) {
      var deskRect = desks.getBoundingClientRect();
      desks.style.setProperty("--split", clamp((height * 0.95 - deskRect.top) / (height * 0.8), 0, 1).toFixed(3));
    }
    if (heroSection) {
      heroSection.style.setProperty("--hero-drop", clamp(window.scrollY / height, 0, 1).toFixed(3));
    }
  }
  window.addEventListener("scroll", onScroll, { passive: true });
  window.addEventListener("resize", onScroll);
  onScroll();

  /* ---------- ticker: drifts by itself, runs faster while you scroll ---------- */
  var rows = Array.prototype.map.call(document.querySelectorAll(".ticker-row"), function (row) {
    return { element: row, speed: Number(row.getAttribute("data-speed")), offset: 0 };
  });
  if (rows.length && !reduce) {
    var lastScroll = window.scrollY;
    var velocity = 0;
    var ticker = function () {
      var scrolled = window.scrollY - lastScroll;
      lastScroll = window.scrollY;
      velocity += (scrolled - velocity) * 0.12;
      rows.forEach(function (row) {
        var half = row.element.scrollWidth / 2;
        row.offset += row.speed * (0.6 + Math.abs(velocity) * 0.9) * (velocity < -0.5 ? -1 : 1);
        if (half > 0) row.offset = ((row.offset % half) + half) % half;
        row.element.style.transform = "translate3d(" + (-row.offset).toFixed(1) + "px,0,0)";
      });
      requestAnimationFrame(ticker);
    };
    requestAnimationFrame(ticker);
  }

  /* ---------- the story scene leans toward the pointer ---------- */
  var stage = document.querySelector(".stage-visual");
  if (stage && scene && !reduce && window.matchMedia("(pointer: fine)").matches) {
    stage.addEventListener("pointermove", function (event) {
      var rect = stage.getBoundingClientRect();
      scene.style.setProperty("--lx", (((event.clientX - rect.left) / rect.width) * 2 - 1).toFixed(3));
      scene.style.setProperty("--ly", (((event.clientY - rect.top) / rect.height) * 2 - 1).toFixed(3));
    });
    stage.addEventListener("pointerleave", function () {
      scene.style.setProperty("--lx", "0");
      scene.style.setProperty("--ly", "0");
    });
  }

  /* ---------- reveal on scroll ---------- */
  var revealed = document.querySelectorAll(".rv");
  if ("IntersectionObserver" in window && !reduce) {
    var revealer = new IntersectionObserver(function (entries) {
      entries.forEach(function (entry) {
        if (entry.isIntersecting) {
          entry.target.classList.add("in");
          revealer.unobserve(entry.target);
        }
      });
    }, { rootMargin: "0px 0px -8% 0px" });
    revealed.forEach(function (element) { revealer.observe(element); });
  } else {
    revealed.forEach(function (element) { element.classList.add("in"); });
  }

  function whenSeen(element, callback) {
    if (!element) return;
    if (!("IntersectionObserver" in window)) { callback(); return; }
    var watcher = new IntersectionObserver(function (entries) {
      if (entries.some(function (entry) { return entry.isIntersecting; })) {
        watcher.disconnect();
        callback();
      }
    }, { threshold: 0.35 });
    watcher.observe(element);
  }

  /* ---------- fleet demo ---------- */
  var demo = document.getElementById("demo");
  if (demo) {
    var projects = [
      ["build-box", "api"], ["build-box", "web"], ["gpu-a", "trainer"],
      ["gpu-a", "datasets"], ["mac-mini", "ios-app"], ["This Mac", "notes"]
    ];
    var buttons = demo.querySelectorAll(".demo-p");
    var panes = demo.querySelectorAll(".demo-pane");
    var crumb = document.getElementById("demo-crumb");
    var status = document.getElementById("demo-status");
    var kept = [0];
    var touched = false;
    var timer = null;

    var select = function (index) {
      var project = projects[index];
      var warm = kept.indexOf(index) !== -1;
      kept = [index].concat(kept.filter(function (other) { return other !== index; })).slice(0, 4);
      buttons.forEach(function (button, buttonIndex) {
        button.classList.toggle("sel", buttonIndex === index);
        button.classList.toggle("kept", kept.indexOf(buttonIndex) !== -1);
      });
      panes.forEach(function (pane, paneIndex) { pane.classList.toggle("on", paneIndex === index); });
      crumb.innerHTML = "<b>" + project[0] + "</b> · " + project[1];
      var name = project[0] + " / " + project[1];
      if (warm || project[0] === "This Mac") {
        status.innerHTML = "switched to " + name + " · <span class=\"warm\">still open · " + (28 + ((index * 17) % 30)) + " ms</span>";
      } else {
        status.innerHTML = "connecting to " + project[0] + " · <span class=\"cold\">connection pre-warmed on hover</span>";
        setTimeout(function () {
          if (buttons[index].classList.contains("sel")) {
            status.innerHTML = "opened " + name + " · <span class=\"warm\">" + (0.6 + index * 0.13).toFixed(1) + " s</span>";
          }
        }, reduce ? 0 : 650);
      }
    };

    buttons.forEach(function (button, index) {
      button.addEventListener("click", function () {
        touched = true;
        if (timer) clearInterval(timer);
        select(index);
      });
    });

    whenSeen(demo, function () {
      if (reduce) return;
      var order = [2, 4, 0, 1, 2, 3, 5, 0];
      var step = 0;
      timer = setInterval(function () {
        if (touched) return clearInterval(timer);
        select(order[step % order.length]);
        step += 1;
      }, 2600);
    });
  }

  /* ---------- Dispatch: the conversation plays once you get to it ---------- */
  var handset = document.getElementById("handset");
  if (handset) {
    var lines = handset.querySelectorAll(".say");
    if (reduce) {
      lines.forEach(function (line) { line.classList.add("shown"); });
    } else {
      whenSeen(handset, function () {
        handset.classList.add("listening");
        lines.forEach(function (line, index) {
          setTimeout(function () { line.classList.add("shown"); }, 500 + index * 1300);
        });
      });
    }
  }

  /* ---------- the terminal types its commands ---------- */
  var output = document.getElementById("term-out");
  if (output) {
    var script = [
      ["yonder hub list", [
        "<span class=\"a\">3f2a91c0  waiting</span>  build-box  claude-acp  Retry backoff",
        "<span class=\"g\">9c41e7b2  running</span>  gpu-a      codex-acp   Eval sweep",
        "07d5aa13  idle     mac-mini   claude-acp  Onboarding copy"
      ]],
      ["yonder hub approve 3f2a91c0", ["Claude wants to run: cargo test -p jobs backoff", "<span class=\"g\">approved once</span>"]],
      ["yonder hub follow 9c41e7b2", ["run 5/6 · lr 1e-3 · val loss 1.86"]]
    ];
    var render = function (upTo, partial) {
      var html = "";
      script.forEach(function (entry, index) {
        if (index > upTo) return;
        var command = index === upTo && partial !== null ? entry[0].slice(0, partial) : entry[0];
        html += "<span class=\"p\">$ </span>" + command;
        if (index < upTo || partial === null) {
          html += "\n<span class=\"o\">" + entry[1].join("\n") + "</span>\n";
        }
      });
      output.innerHTML = html + "<span class=\"caret\"></span>";
    };
    if (reduce) {
      render(script.length - 1, null);
    } else {
      render(0, 0);
      whenSeen(output, function () {
        var command = 0;
        var character = 0;
        var tick = function () {
          if (command >= script.length) return;
          if (character <= script[command][0].length) {
            render(command, character);
            character += 1;
            setTimeout(tick, 38);
          } else {
            render(command, null);
            command += 1;
            character = 0;
            setTimeout(tick, 900);
          }
        };
        tick();
      });
    }
  }
})();
