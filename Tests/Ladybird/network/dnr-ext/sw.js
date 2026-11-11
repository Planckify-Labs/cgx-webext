chrome.declarativeNetRequest.onRuleMatchedDebug.addListener((info) => console.log('matched', info.rule.ruleId, info.request.url));
