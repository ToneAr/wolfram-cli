With[{p = __PREFIX__},
  Module[{
      contexts = Contexts[],
      searchContexts,
      symbols,
      symbolsInContext,
      contextOf,
      shortName,
      item
    },
    searchContexts = DeleteDuplicates[Join[{$Context}, $ContextPath, contexts]];
    symbols =
      If[StringContainsQ[p, "`"],
        Names[StringJoin[ p, "*"]],
        Names[StringJoin["*`", p, "*"]]
   ];
    contextOf[name_] :=
      If[StringContainsQ[name, "`"],
        StringReplace[name, Shortest[___] ~~ "`" ~~ EndOfString :> "`"],
        ""
      ];
    shortName[name_] :=
      If[StringContainsQ[name, "`"],
        StringReplace[name, Shortest[___] ~~ "`" -> ""],
        name
      ];
    item[name_] :=
      StringRiffle[{"symbol", shortName[name], "0", contextOf[name]}, "\t"];
    StringRiffle[
      Take[
        DeleteDuplicates[
          Join[
            item /@ symbols,
            (StringJoin[ "context\t", #, "\t0\t", #])& /@ Select[
              contexts,
              StringStartsQ[#, p]&
            ]
          ]
        ],
        UpTo[500]
      ],
      "\n"
    ]
  ]
]
