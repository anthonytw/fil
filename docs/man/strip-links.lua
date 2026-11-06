-- Pandoc filter for the man pages made from docs/user/: a link to another page of the guide
-- (a relative target) is just its text in a man page, where it leads nowhere. Links to the web
-- stay as "text (URL)".
function Link(el)
  if el.target:match("^https?://") then
    return nil
  end
  return el.content
end
