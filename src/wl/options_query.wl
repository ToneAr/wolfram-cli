Quiet[Check[
  StringRiffle[ToString /@ First /@ Options[ToExpression[TemplateSlot["slotname"]]], "\n"],
  ""
]]
