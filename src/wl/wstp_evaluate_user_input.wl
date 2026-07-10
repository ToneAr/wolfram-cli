Function[
	{input},
	Module[{promptedInput, promptedInputString},
		SetAttributes[{promptedInput, promptedInputString}, HoldAll];
		promptedInput[prompt_] :=
			(
				WriteString[$Output, ToString[Unevaluated[prompt], OutputForm]];
				Input[]
			);
		promptedInputString[prompt_] :=
			(
				WriteString[$Output, ToString[Unevaluated[prompt], OutputForm]];
				InputString[]
			);
		Internal`WithLocalSettings[
			Off[General::shdw],
			ReleaseHold[
				ToExpression[input, InputForm, HoldComplete] /. {
					HoldPattern[Input[prompt_]]       :> promptedInput[prompt],
					HoldPattern[InputString[prompt_]] :> promptedInputString[
						prompt
					]
				}
			],
			On[General::shdw]
		]
	]
]