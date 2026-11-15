---
name: delete-site
description: Handle a request to delete or take down a website - explain what deletion does, then ask the user to confirm it themselves. Use whenever the user wants a site deleted, removed, taken offline or destroyed.
license: MIT OR Apache-2.0
metadata:
  author: sitewright
  version: "1.0"
---

# Delete a site

You cannot delete a site. Only the user can, by typing the site's name when asked. Your
job is to make sure they know what will happen, and then to raise the confirmation.

## Steps

1. **Check it is really deletion they want.** If they said "remove the gallery page" or
   "clear the content", that is an edit, not a deletion: use the edit-site skill. If they
   want an earlier version back, that is `rollback_site`.
2. **Call `delete_site`** (no arguments). This does not delete anything. It shows the user
   a confirmation box in which they must type the site's name.
3. **Tell the user what confirming will do**, briefly:
   - The site goes offline immediately; its address will show "not found".
   - It can be restored for 7 days with `sitewright site restore <name>` on the server.
     After that it is removed for good, including its history.
   - DNS records are not touched. A wildcard record needs no cleanup; a record added just
     for this site can be removed at the DNS host.
   - The name cannot be reused until the 7 days have passed.
4. **Stop.** Do not call `delete_site` again, and do not try other ways to remove the site
   (deleting all its files, publishing an empty page). If the user does not confirm,
   nothing happens, and that is the correct outcome.

## Do not

- Do not delete files to "prepare" for deletion.
- Do not say the site has been deleted. You do not know whether the user confirmed.
- Do not ask the user to type the name to you in chat: the confirmation box is the only
  thing that counts.
