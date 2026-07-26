export default class TangledMarkdownEditor extends HTMLElement {
    static tag = "markdown-editor";

    static define(tag = this.tag) {
        this.tag = tag;

        const name = customElements.getName(this);
        if (name && name !== tag) return console.warn(`${this.name} already defined as <${name}>!`);

        const ce = customElements.get(tag);
        if (ce && ce !== this) return console.warn(`${tag} already defined as ${ce.name}!`);

        customElements.define(tag, this);
    }

    static {
        const tag = new URL(import.meta.url).searchParams.get("tag") || this.tag;
        if (tag != "none") this.define(tag);
    }

    #dragHoverClass = "drag-hover";
    #uploadCounter = 0;
    // cid -> object URL, to preview an image before its blob is committed
    #objectUrls = new Map();

    constructor() {
        super();
        this.textarea = this.querySelector("textarea");
        if (!this.textarea) {
            console.error("textarea is missing in markdown-editor");
            return;
        }

        this.querySelectorAll('[data-md-mode]').forEach(btn => {
            btn.addEventListener("click", () => {
                const mode = btn.dataset.mdMode;
                this.querySelectorAll('[data-md-panel]').forEach(p => {
                    p.classList.toggle('hidden', p.dataset.mdPanel !== mode);
                });
                this.querySelectorAll('[data-md-mode]').forEach(b => {
                    b.classList.toggle('active', b === btn);
                });
            });
        });

        this.textarea.addEventListener("paste", (ev) => this.#onPaste(ev));
        this.textarea.addEventListener("dragover", (ev) => this.#onDragOver(ev));
        this.textarea.addEventListener("dragleave", (ev) => this.#onDragLeave(ev));
        this.textarea.addEventListener("drop", (ev) => this.#onDrop(ev));

        // swap local object URLs into rendered previews (getBlob can't serve uncommitted blobs)
        this.addEventListener("htmx:afterSwap", () => this.#hydratePreview());
    }

    disconnectedCallback() {
        for (const url of this.#objectUrls.values()) URL.revokeObjectURL(url);
        this.#objectUrls.clear();
    }

    #hydratePreview() {
        this.querySelectorAll("[data-md-preview] img[data-blob-cid]").forEach(img => {
            const url = this.#objectUrls.get(img.dataset.blobCid);
            if (url) img.src = url;
        });
    }

    // name of the hidden input carrying blob refs back to the form
    get #blobName() {
        return this.getAttribute("blob-name") || "blobs";
    }

    async insertFile() {
        const input = document.createElement("input");
        input.type = "file";
        input.accept = "image/*";
        input.multiple = true;
        input.addEventListener("change", () => {
            if (!input.files) return;
            for (const file of input.files) {
                this.#handleFile(file);
            }
        });
        input.click();
    }

    /** @param {ClipboardEvent} ev */
    async #onPaste(ev) {
        const dt = ev.clipboardData;
        if (!dt || !dt.files || dt.files.length === 0) return;

        ev.preventDefault();

        for (const file of dt.files) {
            if (!file.type.startsWith("image/")) continue;

            await this.#handleFile(file);
        }
    }

    /** @param {DragEvent} ev */
    async #onDragOver(ev) {
        ev.preventDefault();
        this.classList.add(this.#dragHoverClass);
    }

    /** @param {DragEvent} ev */
    async #onDragLeave(ev) {
        ev.preventDefault();
        this.classList.remove(this.#dragHoverClass);
    }

    /** @param {DragEvent} ev */
    async #onDrop(ev) {
        this.classList.remove(this.#dragHoverClass);

        const dt = ev.dataTransfer;
        if (!dt || !dt.files || dt.files.length === 0) return;

        ev.preventDefault();

        for (const file of dt.files) {
            if (!file.type.startsWith("image/")) continue;

            await this.#handleFile(file);
        }
    }

    /** @param {File} file */
    async #handleFile(file) {
        const textarea = this.textarea;
        if (!textarea) return;

        if (!file || !file.type.startsWith("image/")) {
            console.warn("skipping non-image file", file && file.name);
            return;
        }

        let bytes;
        try {
            bytes = await file.arrayBuffer();
        } catch (e) {
            console.error("failed to read file", e);
            return;
        }
        if (bytes.byteLength === 0) {
            console.error("skipping empty file", file.name);
            return;
        }

        const token = ++this.#uploadCounter;
        const placeholder = `<!-- Uploading "${file.name}" (#${token})... -->`;

        this.#insertTextAtCursor(placeholder);

        let result;
        try {
            result = await this.#upload(bytes, file.type);
        } catch (e) {
            console.error("failed to upload blob", e);
            this.#replaceInTextarea(placeholder, `<!-- Failed to upload "${file.name}": ${e.message} -->`);
            return;
        }

        this.#replaceInTextarea(placeholder, `![Image](${result.uri})`);
        this.#addBlobInput(result.blob);

        // stash a local object URL keyed by cid (echoed back as data-blob-cid) for Preview
        const cid = result.uri.split("/").pop();
        if (cid) {
            const prev = this.#objectUrls.get(cid);
            if (prev) URL.revokeObjectURL(prev);
            this.#objectUrls.set(cid, URL.createObjectURL(new Blob([bytes], { type: file.type })));
        }
    }

    /** @param {string} text */
    #insertTextAtCursor(text) {
        const textarea = this.textarea;
        if (!textarea) return;
        const start = textarea.selectionStart;
        const end = textarea.selectionEnd;

        const before = textarea.value.slice(0, start);
        const after = textarea.value.slice(end);

        // add surrounding newlines if it's mid-line
        if (before && !before.endsWith("\n")) text = "\n\n" + text;
        if (after && !after.startsWith("\n")) text = text + "\n\n";

        textarea.value = before + text + after;

        const newPos = start + text.length;
        textarea.selectionStart = textarea.selectionEnd = newPos;

        this.#fireInput(text);
    }

    /** @param {string} needle @param {string} replacement */
    #replaceInTextarea(needle, replacement) {
        const textarea = this.textarea;
        if (!textarea) return;
        // function replacer so `$` in the filename isn't read as a substitution pattern
        textarea.value = textarea.value.replace(needle, () => replacement);
        this.#fireInput(replacement);
    }

    #fireInput(data = "") {
        const textarea = this.textarea;
        if (!textarea) return;
        textarea.dispatchEvent(
            new InputEvent("input", { bubbles: true, inputType: "insertText", data })
        );
        textarea.dispatchEvent(new Event("change", { bubbles: true }));
    }

    /** @param {object} blob */
    #addBlobInput(blob) {
        const input = document.createElement("input");
        input.type = "hidden";
        input.name = this.#blobName;
        input.value = JSON.stringify(blob);
        this.appendChild(input);
    }

    /** @param {ArrayBuffer} bytes @param {string} contentType */
    async #upload(bytes, contentType) {
        const host = this.getAttribute("host") ?? "";
        const res = await fetch(host + "/markup/upload", {
            method: "POST",
            body: bytes,
            headers: {
                "Content-Type": contentType,
            },
        });
        if (!res.ok) {
            let msg = `upload failed (${res.status})`;
            try {
                const err = await res.json();
                if (err && err.error) msg = err.error;
            } catch {
                // non-JSON error body; keep the status-based message
            }
            throw new Error(msg);
        }
        // { blob, did, uri }
        return await res.json();
    }
}
