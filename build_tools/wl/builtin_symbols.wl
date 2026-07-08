Quiet[Module[{freqs, symbols, score},
  freqs = Lookup[#, "All", 0]& /@ DeleteMissing[Association@(Rule@@@WolframLanguageData[All,{"Name","Frequencies"}])];
  symbols = Select[Names["System`*"], StringMatchQ[#, RegularExpression["[A-Za-z$][A-Za-z0-9$]*"]]&];
  symbols = SortBy[symbols, {-Lookup[freqs, #, 0], #}&];
  score[name_] := ToString[Round[10000 Lookup[freqs, name, 0]]];
  StringRiffle[(# <> "\t" <> score[#]) & /@ symbols, "\n"]
]]
