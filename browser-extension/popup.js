const state=document.querySelector("#state"); chrome.storage.local.get("taceta_owned_scope").then(({taceta_owned_scope:s})=>{state.textContent=s?`接続済み / tab ${s.tabId}`:"待機中";});
