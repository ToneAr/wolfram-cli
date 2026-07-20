Function[
	{source, scriptCommandLine, evaluationEnvironment, inputFileName},
	Module[{
			stream,
			held,
			pending = {},
			result = "",
			processResult,
			runCommand,
			splitCompoundExpressions,
			promptedInput,
			promptedInputString
		},
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
		runCommand[command_String] :=
			RunProcess[
				If[$OperatingSystem === "Windows",
					{"cmd.exe", "/c", command},
					{"/bin/sh", "-c", command}
				],
				All
			];
		splitCompoundExpressions[expression_] :=
			If[MatchQ[expression, HoldComplete[CompoundExpression[___]]],
				Flatten[
					splitCompoundExpressions /@ (
						Map[HoldComplete, expression, {2}] /. HoldComplete[
								CompoundExpression[expressions___]
							] :> {expressions}
					)
				],
				{expression}
			];
		stream = StringToStream[source];
		Internal`WithLocalSettings[
			Null,
			Block[{
					$ScriptCommandLine = scriptCommandLine,
					$EvaluationEnvironment =
						If[StringQ[evaluationEnvironment],
							evaluationEnvironment,
							$EvaluationEnvironment
						],
					System`Private`$InputFileName = inputFileName
				},
				While[
					True,
					If[pending === {},
						held = Read[stream, HoldComplete[Expression]];
						If[held === EndOfFile, Break[]];
						pending = splitCompoundExpressions[held]
					];
					held = First[pending];
					pending = Rest[pending];
					held =
						held /. {
							HoldPattern[Input[prompt_]]       :> promptedInput[
								prompt
							],
							HoldPattern[InputString[prompt_]] :> promptedInputString[
								prompt
							]
						};
					If[MatchQ[held, HoldComplete[Run[_String]]],
						processResult =
							ReleaseHold[
								held /. HoldComplete[
										Run[command_String]
									] :> HoldComplete[runCommand[command]]
							];
						If[processResult["StandardOutput"] =!= "",
							Print[
								StringTrim[
									processResult["StandardOutput"],
									"\n"
								]
							]
						];
						Continue[]
					];
					If[MatchQ[held, HoldComplete[Return[Run[_String]]]],
						processResult =
							ReleaseHold[
								held /. HoldComplete[
										Return[Run[command_String]]
									] :> HoldComplete[runCommand[command]]
							];
						If[processResult["StandardOutput"] =!= "",
							Print[
								StringTrim[
									processResult["StandardOutput"],
									"\n"
								]
							]
						];
						result = processResult["ExitCode"];
						Break[]
					];
					If[MatchQ[held, HoldComplete[Return[_]]],
						result =
							ReleaseHold[
								held /. HoldComplete[
										Return[value_]
									] :> HoldComplete[value]
							];
						Break[]
					];
					ReleaseHold[held]
				];
				result
			],
			Close[stream]
		]
	]
]