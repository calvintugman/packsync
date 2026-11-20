import { useEffect, useState } from "react";
import { openExternalUrl } from "../lib/external";
import { useStore } from "../state/context";
import { Icon } from "../ui/Icon";

const BIO_TEXT = "Data Science student";

function prefersReducedMotion(): boolean {
  if (typeof window === "undefined" || typeof window.matchMedia !== "function") return false;
  return window.matchMedia("(prefers-reduced-motion: reduce)").matches;
}

export function AboutScreen() {
  const store = useStore();
  const [typed, setTyped] = useState(() => (prefersReducedMotion() ? BIO_TEXT : ""));

  useEffect(() => {
    if (prefersReducedMotion()) {
      return;
    }
    let idx = 0;
    const id = setInterval(() => {
      idx += 1;
      setTyped(BIO_TEXT.slice(0, idx));
      if (idx >= BIO_TEXT.length) clearInterval(id);
    }, 34);
    return () => clearInterval(id);
  }, []);

  const openLink = (e: React.MouseEvent<HTMLAnchorElement>, url: string) => {
    e.preventDefault();
    void openExternalUrl(store, url);
  };

  return (
    <div className="about-stage">
      <div className="about-bg" aria-hidden="true">
        <div className="about-curve" />
        <div className="about-island" />
      </div>

      <section className="about-profile" aria-label="About the developer">
        <div className="about-avatar-wrap">
          <img className="about-avatar" src="/dev-avatar.jpg" alt="calvintugman avatar" />
        </div>

        <h1 className="about-handle">calvintugman</h1>

        <div className="about-badges" aria-label="Profile badges">
          <Icon name="code" size={16} />
          <Icon name="sparkles" size={16} />
          <Icon name="star" size={16} />
          <Icon name="grid" size={16} />
          <Icon name="shield" size={16} />
          <Icon name="cloud" size={16} />
        </div>

        <p className="about-bio" aria-label={BIO_TEXT}>
          <span aria-hidden="true">{typed}</span>
          <span className="about-caret" aria-hidden="true">
            |
          </span>
          <span className="sr-only">{BIO_TEXT}</span>
        </p>

        <div className="about-socials">
          <a
            className="about-social-icon"
            href="https://github.com/calvintugman"
            target="_blank"
            rel="noopener noreferrer"
            aria-label="GitHub"
            onClick={(e) => openLink(e, "https://github.com/calvintugman")}
          >
            <Icon name="github" size={24} />
          </a>
        </div>

        <div className="about-links">
          <a
            className="about-link-card"
            href="https://github.com/calvintugman/packsync"
            target="_blank"
            rel="noopener noreferrer"
            onClick={(e) => openLink(e, "https://github.com/calvintugman/packsync")}
          >
            <span className="about-link-title">
              PackSync
              <span className="about-lang-pill">Rust · TypeScript</span>
              <Icon name="external" size={14} />
            </span>
            <span className="about-link-url">
              <Icon name="link" size={13} />
              https://github.com/calvintugman/packsync
            </span>
          </a>
        </div>
      </section>
    </div>
  );
}
