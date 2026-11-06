-- Pandoc filter for the man pages made from docs/user/. Links lead nowhere in a man page:
--   * the example files are installed as share/doc/fil/NAME.example.toml, and the text says so;
--   * a link within the page is just its text;
--   * a link to another page of the guide is its text plus "(in the guide)";
--   * links to the web stay as "text (URL)".
function Link(el)
  if el.target:match("^https?://") then
    return nil
  end
  local kind = el.target:match("^examples/(%a+)%.toml$")
  if kind then
    return pandoc.Str(kind .. ".example.toml (in share/doc/fil)")
  end
  if el.target:match("^#") then
    return el.content
  end
  local out = pandoc.List(el.content)
  out:insert(pandoc.Str(" (in the guide)"))
  return out
end
