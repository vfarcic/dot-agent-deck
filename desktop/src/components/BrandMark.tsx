import logoUrl from "../assets/logo.svg";

/*
  The project mark at the top of the rail, shared by the deck and the overview
  so the two screens cannot drift apart. `assets/logo.svg` is a copy of
  assets/brand/logo.svg written by scripts/brand-icons.sh -- edit the master
  and rerun that, never this copy (docs/develop/brand-assets.md).
*/
export function BrandMark() {
  return <img className="brand-mark" src={logoUrl} alt="Agent Deck" width={36} height={36} />;
}
