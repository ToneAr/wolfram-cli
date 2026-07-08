Module[{promptedInputString, promptedInput},
  SetAttributes[{promptedInputString, promptedInput}, HoldAll];
  promptedInputString[prompt_] := (
    WriteString[$Output, ToString[Unevaluated[prompt], OutputForm]];
    InputString[]
  );
  promptedInput[prompt_] := ToExpression[promptedInputString[prompt]];
  ToString[
    __INPUT_EXPR__ /. {
      HoldPattern[InputString[prompt_]] :> promptedInputString[prompt],
      HoldPattern[Input[prompt_]] :> promptedInput[prompt]
    },
    InputForm
  ]
]
