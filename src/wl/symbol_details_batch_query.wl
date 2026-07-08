Module[{names = {__NAMES__}, contextOf, usageOf, item},
  contextOf[name_] :=
    If[StringContainsQ[name, "`"],
      StringReplace[name, Shortest[___] ~~ "`" ~~ EndOfString :> "`"],
      ""
    ];
  usageOf[name_] := Module[{raw},
    raw = Quiet[Check[ToExpression[name <> "::usage"], ""]];
    If[StringQ[raw],
      StringTrim[StringReplace[ToString[raw, OutputForm], {"\t" -> " ", "\n" -> " ", "\r" -> " "}]],
      ""
    ]
  ];
  item[name_] := StringRiffle[{name, contextOf[name], usageOf[name]}, "\t"];
  StringRiffle[item /@ names, "\n"]
]
