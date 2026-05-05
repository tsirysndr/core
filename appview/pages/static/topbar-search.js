(() => {
    if (window._navSearchReady) return;
    window._navSearchReady = true;

    const $ = (id) => document.getElementById(id);

    const submitFromInput = (input) => {
        const query = input.value.trim();
        if (query)
            window.location.href = `/search?q=${encodeURIComponent(query)}`;
    };

    // mobile-related code
    let savedScrollY = 0;
    let touchMoveHandler = null;

    const updateOverlayHeight = () => {
        const overlay = $("mobile-search-overlay");
        if (!overlay) return;

        const layoutHeight = window.innerHeight;
        const visibleHeight = window.visualViewport?.height ?? layoutHeight;

        overlay.style.height = `${layoutHeight}px`;
        overlay.style.top = "0px";

        const spacer = $("mobile-search-spacer");
        if (spacer)
            spacer.style.height = `${Math.max(0, layoutHeight - visibleHeight)}px`;
    };

    const openMobile = () => {
        const overlay = $("mobile-search-overlay");
        if (!overlay || overlay.classList.contains("opacity-100")) return;

        overlay.classList.remove("opacity-0", "pointer-events-none");
        overlay.classList.add("opacity-100", "pointer-events-auto");
        overlay.setAttribute("aria-hidden", "false");

        savedScrollY = window.scrollY;
        Object.assign(document.body.style, {
            position: "fixed",
            top: `-${savedScrollY}px`,
            width: "100%",
        });
        updateOverlayHeight();

        if (window.visualViewport) {
            window.visualViewport.addEventListener(
                "resize",
                updateOverlayHeight,
            );
        }

        $("mobile-search-input")?.focus({ preventScroll: true });

        const results = $("mobile-search-results");
        if (results && !touchMoveHandler) {
            touchMoveHandler = (e) => e.preventDefault();
            results.addEventListener("touchmove", touchMoveHandler, {
                passive: false,
            });
        }
    };

    const closeMobile = () => {
        const overlay = $("mobile-search-overlay");
        if (!overlay) return;

        overlay.classList.remove("opacity-100", "pointer-events-auto");
        overlay.classList.add("opacity-0", "pointer-events-none");
        overlay.setAttribute("aria-hidden", "true");
        overlay.style.height = "";
        overlay.style.top = "";

        const spacer = $("mobile-search-spacer");
        if (spacer) {
            spacer.style.height = "";
            spacer.classList.add("hidden");
        }

        if (window.visualViewport) {
            window.visualViewport.removeEventListener(
                "resize",
                updateOverlayHeight,
            );
        }

        Object.assign(document.body.style, {
            position: "",
            top: "",
            width: "",
        });
        window.scrollTo(0, savedScrollY);

        const input = $("mobile-search-input");
        if (input) {
            input.value = "";
            input.blur();
        }

        $("mobile-search-results")?.replaceChildren();
    };

    // desktop-related things
    const clearDesktop = () => {
        $("topbar-search-results")?.replaceChildren();

        const box = $("topbar-search-box");
        box?.classList.add("rounded");
        box?.classList.remove("rounded-t");
    };

    // events
    document.addEventListener("click", ({ target }) => {
        // mobile: open/close overlay via data-action buttons
        const action = target
            .closest("[data-action]")
            ?.getAttribute("data-action");
        if (action === "open-mobile-search") {
            openMobile();
            return;
        }
        if (action === "close-mobile-search") {
            closeMobile();
            return;
        }

        // desktop: clicking outside the search container clears results
        const container = $("topbar-search-container");
        if (container && !container.contains(target)) clearDesktop();
    });

    // desktop: defer so a click on a result fires before results are cleared
    document.addEventListener("focusout", ({ target, relatedTarget }) => {
        const container = $("topbar-search-container");
        if (container?.contains(target) && !container.contains(relatedTarget)) {
            setTimeout(clearDesktop, 0);
        }
    });

    document.addEventListener("htmx:afterSwap", ({ detail: { target } }) => {
        if (!target) return;

        // desktop: toggle rounded corners based on whether results are open
        if (target.id === "topbar-search-results") {
            const box = $("topbar-search-box");
            const open = target.children.length > 0;
            box?.classList.toggle("rounded", !open);
            box?.classList.toggle("rounded-t", open);
            return;
        }

        // mobile: restore touch listener and show spacer when results arrive
        if (target.id === "mobile-search-results") {
            if (touchMoveHandler) {
                target.removeEventListener("touchmove", touchMoveHandler);
                touchMoveHandler = null;
            }

            const hasResults = !!target.querySelector("[data-results-footer]");
            $("mobile-search-spacer")?.classList.toggle("hidden", !hasResults);
        }
    });

    document.addEventListener("keydown", (e) => {
        const { key, metaKey, ctrlKey } = e;
        const input = $("topbar-search-input");
        const results = $("topbar-search-results");
        const mobileOverlay = $("mobile-search-overlay");
        const mobileInput = $("mobile-search-input");
        const active = document.activeElement;

        // desktop: ⌘K / Ctrl+K focuses the search input
        if ((metaKey || ctrlKey) && key === "k") {
            e.preventDefault();
            input?.focus();
            input?.select();
            return;
        }

        if (key === "Enter") {
            if (active === input) {
                e.preventDefault();
                submitFromInput(input);
                return;
            } // desktop
            if (active === mobileInput) {
                e.preventDefault();
                submitFromInput(mobileInput);
                return;
            } // mobile
        }

        if (key === "Escape") {
            // mobile: close the overlay
            if (
                mobileOverlay &&
                !mobileOverlay.classList.contains("opacity-0")
            ) {
                e.preventDefault();
                closeMobile();
                return;
            }
            // desktop: clear results and blur
            if (input) {
                const links = results
                    ? [...results.querySelectorAll("[data-nav-result]")]
                    : [];
                if (active === input || links.includes(active)) {
                    e.preventDefault();
                    clearDesktop();
                    input.blur();
                }
            }
        }

        // desktop: arrow key navigation through results
        if (!input || !results) return;

        const links = [...results.querySelectorAll("[data-nav-result]")];
        const inputFocused = active === input;
        const focusedIndex = links.indexOf(active);

        if (key === "ArrowDown") {
            if (inputFocused && links.length) {
                e.preventDefault();
                links[0].focus();
            } else if (focusedIndex >= 0 && focusedIndex < links.length - 1) {
                e.preventDefault();
                links[focusedIndex + 1].focus();
            }
        }

        if (key === "ArrowUp") {
            if (focusedIndex === 0) {
                e.preventDefault();
                input.focus();
            } else if (focusedIndex > 0) {
                e.preventDefault();
                links[focusedIndex - 1].focus();
            }
        }
    });
})();
