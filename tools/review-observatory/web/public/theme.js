try {
  const theme = localStorage.getItem("observatory-theme");
  document.documentElement.classList.toggle(
    "dark",
    theme === "dark" ||
      (!theme && matchMedia("(prefers-color-scheme: dark)").matches),
  );
} catch {}
