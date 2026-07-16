Function[
	{serviceLinkName, preemptiveLinkName, protocol},
	Module[{
			secondaryLinkNames = {serviceLinkName, preemptiveLinkName}
		},
		Block[{MathLink`CreateFrontEndLink},
			MathLink`CreateFrontEndLink[] :=
				Module[{link},
					link =
						LinkCreate[
							First[secondaryLinkNames],
							LinkMode     -> Connect,
							LinkProtocol -> protocol
						];
					MathLink`LinkSetPrintFullSymbols[link, True];
					secondaryLinkNames = Rest[secondaryLinkNames];
					link
				];
			MathLink`CreateFrontEndLinks[]
		];
		LinkActivate[MathLink`$ServiceLink];
		LinkActivate[MathLink`$PreemptiveLink];
		MathLink`AddSharingLink[
			MathLink`$PreemptiveLink,
			MathLink`AllowPreemptive -> True
		];
		MathLink`SetTerminating[$ParentLink, True];
		"ok"
	]
]