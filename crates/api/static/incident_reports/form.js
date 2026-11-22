(() => {
  "use strict";

  const form = document.getElementById("incident");
  const entry = document.getElementById("entry");
  const receipt = document.getElementById("receipt");
  const error = document.getElementById("error");
  const submit = document.getElementById("submit");
  const textFields = ["title", "description", "service"];
  let attempt = null;
  let busy = false;

  function showError(message) {
    error.hidden = false;
    error.textContent = message;
  }

  function setBusy(value) {
    busy = value;
    form.setAttribute("aria-busy", String(value));
    for (const control of form.querySelectorAll("input, textarea, button")) {
      control.disabled = value;
    }
    submit.textContent = value ? "Отправляем…" : "Отправить";
  }

  function readAnswers() {
    const answers = {};
    for (const name of textFields) {
      const input = form.elements.namedItem(name);
      answers[name] = input.value.trim();
      if (!answers[name]) {
        input.setCustomValidity("Заполните это поле.");
        input.reportValidity();
        return null;
      }
    }
    answers.priority = form.elements.namedItem("priority").value;
    const duration = form.elements.namedItem("allocated_minutes");
    if (duration.value !== "") answers.allocated_minutes = duration.valueAsNumber;
    return answers;
  }

  function failureMessage(status) {
    if (status === 400 || status === 413 || status === 422) {
      return "Проверьте поля и попробуйте ещё раз.";
    }
    if (status === 429) {
      return "Слишком много запросов. Подождите немного и попробуйте ещё раз.";
    }
    return "Не удалось подтвердить отправку. Ваши ответы остались в форме. Попробуйте ещё раз.";
  }

  form.addEventListener("input", (event) => {
    if (event.target.setCustomValidity) event.target.setCustomValidity("");
    error.hidden = true;
  });

  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    if (busy) return;
    const answers = readAnswers();
    if (!answers) return;

    const contents = JSON.stringify(answers);
    if (!attempt || attempt.contents !== contents) {
      attempt = { contents, id: crypto.randomUUID() };
    }
    error.hidden = true;
    setBusy(true);
    const controller = new AbortController();
    const timeout = window.setTimeout(() => controller.abort(), 15000);
    try {
      const response = await fetch(form.action, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ submission_id: attempt.id, ...answers }),
        signal: controller.signal,
      });
      if (response.status === 409) {
        attempt = null;
        showError("Этот номер отправки уже использован. Проверьте ответы и отправьте заявку ещё раз.");
        return;
      }
      if (response.status !== 201 && response.status !== 200) {
        showError(failureMessage(response.status));
        return;
      }
      const result = await response.json();
      if (typeof result.id !== "string" || !/^INC-FORM-[1-9]\d*$/.test(result.id) || typeof result.created !== "boolean") {
        showError(failureMessage(0));
        return;
      }
      document.getElementById("receipt-number").textContent = "Номер заявки — " + result.id + ".";
      form.reset();
      attempt = null;
      entry.hidden = true;
      receipt.hidden = false;
      document.getElementById("receipt-title").focus();
    } catch {
      showError(failureMessage(0));
    } finally {
      window.clearTimeout(timeout);
      setBusy(false);
    }
  });

  document.getElementById("again").addEventListener("click", () => {
    receipt.hidden = true;
    entry.hidden = false;
    document.getElementById("title").focus();
  });
  submit.disabled = false;
})();
